// 앱 전역 상태(zustand). 메모리에만 둡니다: 메시지·휘발성 내용은 어떤 저장소에도 쓰지 않고,
// UI 설정(테마·패널·마지막 일반 채널 ID·포럼 보기·콘텐츠 뷰어 필터·라이브러리 보기·알림·신뢰 경고 건너뛰기·내 권한 템플릿)만 transport.savePrefs로 저장합니다.
// 데몬 알림은 화면 프레임마다 한 번 모아서 적용합니다(requestAnimationFrame).

import { create } from 'zustand';
import { ApiClient, RpcError, getClient, type DaemonState, type DataMovePlan, type ScreenSource } from '../api/client';
import {
  ErrorCode,
  type AttachmentInput,
  type BlobId,
  type ChannelId,
  type ChannelKind,
  type ChannelView,
  type EventId,
  type ForumSort,
  type GapView,
  type IdentityView,
  type MessageSearchParams,
  type MessageView,
  type Notification,
  type PresenceSetParams,
  type RateLimitView,
  type RateScope,
  type SpaceId,
  type SpaceView,
  type SupporterStatus,
  type SortOrder,
  type TagMatch,
  type UserId,
  type BlockedView,
  type BlockedDeviceView,
  type DeviceId,
  type DeviceLinkRequestView,
  type DeviceLinkStatus,
  type VoiceParticipant,
} from '../api/types';
import { appLinkKind, channelLink } from '../lib/links';
import { ANON_CALL_ENDED, ANON_VOICE_OFF_WARNING, anonCallEnded } from '../lib/anon';
import { isP2pLocked, planChannelMove, type DropWhere } from '../lib/channel';
import { DEFAULT_LIBRARY, sanitizeLibrary, type LibraryPrefs } from '../supporter/library';
import { rememberRecent } from '../lib/misc';
import { sanitizeRules, type NotifyRules } from '../lib/notify';
import { sanitizeTemplates, type PermTemplate } from '../lib/perms';
import { withLimit } from '../lib/ratelimit';
import { compareMsg, gapKey, prependOlder, upsertSorted } from '../lib/rows';
import { parseSearch } from '../lib/search';
import type { TrustTarget } from '../lib/trust';
import { DEFAULT_VIEWER, sanitizeViewer, type ViewerItem, type ViewerPrefs } from '../supporter/viewer';
import { TOR_SCREEN_WARNING, VoiceSession, type ContentHint, type ScreenLayer, type VoiceSnapshot } from '../voice/session';
import { stopFakeScreens } from '../voice/fake';
import { needsTorScreen } from '../voice/policy';
import { DEFAULT_MEDIA, sanitizeMedia, type MediaPrefs } from '../voice/media';
import { isLang, t, tc, type Lang } from '../i18n';
import {
  applyBatch,
  chKey,
  emptyChannel,
  mergeGaps,
  pruneExpired,
  pruneTyping,
  pruneReplyTo,
  type ApplyState,
  type ChannelMsgs,
  type Effect,
  type ToastKind,
} from './apply';

export { chKey } from './apply';

export type CrewTab = 'overview' | 'roles' | 'members' | 'invites' | 'emoji' | 'seeders' | 'moderation';
export type UserTab =
  | 'profile'
  | 'devices'
  | 'supporter'
  | 'privacy'
  | 'sync'
  | 'network'
  | 'tokens'
  | 'audit'
  | 'confirm'
  | 'diag'
  | 'appearance'
  | 'notify'
  | 'storage'
  | 'media'
  | 'about';

export interface AttachmentRef {
  space: SpaceId;
  blob: string;
  keyFrom: EventId;
  mime: string;
  name: string;
  volatile: boolean;
  /** 원본 메시지(만료되면 라이트박스를 닫기 위해) */
  msgKey: string;
  messageId: EventId;
  thumbnail?: string;
}

export type Modal =
  | { kind: 'addFriend'; url?: string }
  | { kind: 'friendProfile'; user: UserId; tab?: 'settings' }
  | { kind: 'createCrew' }
  | { kind: 'joinCrew'; url?: string }
  | { kind: 'convertCrew'; space: SpaceId }
  | { kind: 'channelCreate'; space: SpaceId; parent?: ChannelId | null; channelKind?: ChannelKind }
  | { kind: 'channelSettings'; space: SpaceId; channel: ChannelId }
  | { kind: 'crewSettings'; space: SpaceId; tab?: CrewTab }
  | { kind: 'userSettings'; tab?: UserTab }
  | { kind: 'image'; att: AttachmentRef }
  /** 채널 콘텐츠 뷰어(글·이미지·영상을 하나씩 넘겨 보기). */
  | { kind: 'viewer'; space: SpaceId; channel: ChannelId }
  /** 후원자 전용 기능(라이브러리·콘텐츠 뷰어)을 눌렀는데 후원이 확인되지 않음. */
  | { kind: 'supporter' }
  /** 라이브러리에서 고른 파일을 콘텐츠 뷰어로(정해진 목록, `start`는 처음 볼 항목 키). */
  | { kind: 'fileViewer'; items: ViewerItem[]; start?: string }
  | { kind: 'report'; space: SpaceId; channel: ChannelId; message: MessageView }
  | { kind: 'ban'; space: SpaceId; user: UserId }
  /** 크루·채널·사람별 속도 제한(이 기기만의 설정). `back`이 있으면 닫을 때 그 창으로 돌아갑니다(프로필·설정에서 연 경우). */
  | { kind: 'rateLimit'; scope: RateScope; back?: Modal }
  /**
   * 신뢰 스위치를 켤 때의 IP 노출 경고. 연 창(`back`: 상대 프로필·크루 설정) 위에 겹쳐 띄우고, 닫으면 그 창으로 돌아갑니다.
   * `onConfirm`의 `skip`은 ‘다시 보지 않기’.
   */
  | {
      kind: 'trustWarning';
      target: TrustTarget;
      back?: Modal;
      onConfirm: (skip: boolean) => Promise<void>;
      onCancel: () => void;
    }
  /**
   * 확인 창. 다른 창(설정 등) 위에서 열면 그 창(`back`) 위에 겹쳐 띄우고, 닫으면 그 창이 그대로 남습니다. 빠뜨리면
   * 여는 순간의 창을 저절로 `back`으로 삼고, 확인한 뒤 아래 창도 닫아야 하면(채널 삭제 등) `null`을 줍니다.
   */
  | {
      kind: 'confirm';
      title: string;
      body: string;
      danger?: boolean;
      confirmLabel?: string;
      back?: Modal | null;
      onConfirm: () => void | Promise<void>;
    }
  /** ffmpeg 인코더로 공유할 창·모니터 고르기. 닫으면 `onCancel`. */
  | { kind: 'screenSource'; onPick: (s: ScreenSource) => void; onCancel: () => void }
  /** 데이터 폴더 바꾸기 확인(크기·예상 시간, 옮길지 고르기). */
  | { kind: 'dataMove'; plan: DataMovePlan };

export interface Toast {
  id: number;
  kind: ToastKind;
  text: string;
}

export interface Prefs {
  theme: 'dark' | 'light';
  memberList: boolean;
  /** 공간별 마지막 일반 채널(휘발성 채널은 기억하지 않음) */
  lastChannel: Record<SpaceId, ChannelId>;
  bannerCollapsed: boolean;
  /** OS 알림(src-tauri/src/notify.rs가 같은 키를 읽습니다). */
  notify: boolean;
  /** 크루 메시지 알림: 모든 메시지(글타래 답글은 멘션만) / 나를 멘션한 메시지만 / 끄기. 1대1은 늘 모든 메시지. */
  notifyCrew: NotifyCrew;
  /** 알림에 메시지 내용 미리보기(끄면 보낸 사람·채널만. 윈도 알림 센터는 알림 내용을 디스크에 남깁니다). */
  notifyPreview: boolean;
  /** 크루·채널별 알림 수준·알림 끄기(lib/notify.ts, 크루·채널 오른쪽 클릭 메뉴). */
  notifyRules: NotifyRules;
  /** 신뢰 스위치를 켤 때 IP 노출 경고를 건너뜀(경고의 ‘다시 보지 않기’, 설정 → 네트워크에서 되돌림). */
  skipTrustWarning: boolean;
  /** 창을 닫으면 트레이로 내리고 앱은 계속 돎(src-tauri/src/lib.rs가 같은 키를 읽습니다). */
  closeToTray: boolean;
  /** 윈도에 로그인하면 앱을 켬(src-tauri/src/lib.rs가 같은 키를 읽어 Run 키에 등록합니다). */
  autoStart: boolean;
  /** 로그인할 때 켜면 창 없이 트레이로만 켬(src-tauri/src/lib.rs가 같은 키를 읽습니다). */
  autoStartToTray: boolean;
  /** 역할 편집에서 저장한 내 권한 템플릿(이름·권한 이름만). */
  permTemplates: PermTemplate[];
  /** 포럼 채널별 보기(정렬·방향·목록/갤러리·태그 맞춤). 키는 `공간/채널`이고, 고른 태그는 기억하지 않습니다. */
  forumView: Record<string, ForumViewPrefs>;
  /** 음성·화면공유 세부 설정(내가 보내는 것, 이 기기만). */
  media: MediaPrefs;
  /** 검색 결과 한 쪽에 보일 글 수(검색 결과 오른쪽 위 입력칸). */
  searchPageSize: number;
  /** 채널 콘텐츠 뷰어의 필터·정렬·이미지 맞춤(모든 채널에 같음, 보던 위치는 넣지 않음). */
  viewer: ViewerPrefs;
  /** 라이브러리 탭의 보기·정렬·분류(고른 폴더·파일은 넣지 않음). */
  library: LibraryPrefs;
  /** 보내는 첨부의 메타데이터(촬영 위치·기기·시각 등)를 지움(설정 → 개인정보, 기본 켬). */
  stripMetadata: boolean;
  /** 화면 언어(설정 → 모양). 없으면 OS 언어를 따릅니다. main.tsx·src-tauri가 같은 키를 읽습니다. */
  lang?: Lang;
}

export interface ForumViewPrefs {
  sort: ForumSort;
  order: SortOrder;
  view: 'list' | 'gallery';
  match: TagMatch;
}

export const DEFAULT_FORUM_VIEW: ForumViewPrefs = { sort: 'activity', order: 'desc', view: 'list', match: 'any' };
/** 포럼 보기를 기억하는 채널 수(설정 파일 64KiB 제한 안에 들게). */
export const MAX_FORUM_VIEWS = 100;

export type NotifyCrew = 'all' | 'mentions' | 'off';

const DEFAULT_PREFS: Prefs = {
  theme: 'dark',
  memberList: true,
  lastChannel: {},
  bannerCollapsed: false,
  notify: true,
  notifyCrew: 'all',
  notifyPreview: true,
  notifyRules: {},
  skipTrustWarning: false,
  closeToTray: true,
  autoStart: true,
  autoStartToTray: false,
  permTemplates: [],
  forumView: {},
  media: DEFAULT_MEDIA,
  searchPageSize: 50,
  viewer: DEFAULT_VIEWER,
  library: DEFAULT_LIBRARY,
  stripMetadata: true,
};

/** 검색 결과 쪽 크기 범위(데몬 상한 500). */
export const SEARCH_PAGE_MIN = 1;
export const SEARCH_PAGE_MAX = 500;

/** 검색 결과 한 쪽. `total`은 조건에 맞는 전체 글 수(상한 없음)이고 `offset`은 이 쪽의 시작 위치입니다. */
export interface SearchState {
  /** 입력한 검색어 그대로(표시용). */
  query: string;
  results: MessageView[];
  /** 쪽을 불러오는 중(이전 쪽은 그대로 보여 줌). */
  loading: boolean;
  /** from:으로 고른 사람의 이름. */
  author?: string;
  /** 그 이름의 멤버가 없거나 검색이 실패했을 때. */
  error?: string;
  total: number;
  offset: number;
  /** 데몬에서 걸린 시간(ms). */
  tookMs?: number;
  /** 다른 쪽을 부를 때 다시 쓰는 데몬 인자. */
  req?: Omit<MessageSearchParams, 'offset' | 'limit'>;
}

/** 검색 요청 차례 번호(`searchPage`). */
let searchSeq = 0;

export type Phase = 'loading' | 'onboarding' | 'ready';

export type Selection = { space: 'home'; channel: null } | { space: SpaceId; channel: ChannelId | null };

export interface ProtectionState {
  requested: boolean;
  supported: boolean | null;
}

export type MobilePane = 'list' | 'chat' | 'members';

export interface State extends ApplyState {
  client: ApiClient | null;
  phase: Phase;
  daemon: DaemonState;
  identity: IdentityView | null;
  selected: Selection;
  /** 라이브러리 탭을 보는 중(그동안 `selected`는 홈이라 어떤 채널도 읽음으로 바뀌지 않음). */
  library: boolean;
  /** 좁은 화면(휴대폰)에서 보이는 칸. 넓은 화면은 모든 칸을 함께 보여 줘 이 값을 쓰지 않습니다. */
  mobilePane: MobilePane;
  homeTab: 'online' | 'all' | 'pending' | 'add';
  modal: Modal | null;
  toasts: Toast[];
  prefs: Prefs;
  replyTo: Record<string, MessageView | undefined>;
  editing: { key: string; id: EventId } | null;
  voice: VoiceSnapshot | null;
  voiceJoining: string | null;
  /** 원격 화면 스트림이 바뀔 때 다시 그리기 위한 값 */
  voiceTick: number;
  protection: ProtectionState;
  /** 검색 결과(한 쪽씩). */
  search: SearchState | null;
  /** 내가 차단한 사람(user.blocked). 모더레이션 전 데몬이면 null. */
  blocked: BlockedView[] | null;
  /** 내가 차단한 남의 기기(user.blocked_devices). 기기 차단 전 데몬이면 null. */
  blockedDevices: BlockedDeviceView[] | null;
  /** 열린 메시지 목록이 찾아 보여 줄 메시지(신고 화면의 ‘메시지 보기’). */
  /** 이 채널(key)에서 찾아 보여 줄 메시지. 포럼이면 `post`(그 게시글을 열고 그 안에서 찾음). */
  jump: { key: string; id: EventId; post?: EventId } | null;
  /** 입력창에 커서를 둘 요청(답장 단추). 목록 키와 요청마다 늘어나는 번호(같은 메시지에 다시 눌러도 다시 잡게). */
  composerFocus: { key: string; n: number } | null;
  /** 이 앱 입력창에서 방금 보낸 메시지. 그 목록을 보던 위치와 상관없이 맨 아래로 내립니다(내용은 담지 않음). */
  sent: { key: string; id: EventId; at: number } | null;
  /** 첫 동기화 화면을 이번 실행에서 넘김. */
  firstSyncSkipped: boolean;
  /** 내 링크로 들어와 승인을 기다리는 새 기기. 참이면 허용 창을 띄웁니다. */
  deviceLinkRequest: DeviceLinkRequestView | null;
  /** 새 기기 쪽: 쓰던 기기의 판단. 연결 화면이 읽고 지웁니다. */
  deviceLinkResult: DeviceLinkStatus | null;
  /** OS가 넘긴 기기 연결 링크. 연결 화면이 받아 칸에 채웁니다. */
  deviceLinkUrl: string | null;
  /** 후원자 확인(`supporter.status`). null이면 아직 모르거나 후원 확인을 모르는 데몬이라 막지 않습니다. */
  supporter: SupporterStatus | null;
  /** 누군가의 뱃지가 바뀜(`badges.updated`). 열린 프로필이 뱃지를 다시 읽는 신호. */
  badgesTick: number;
  /** 크루·채널·사람별 속도 제한(`rate.limits`). 아직 읽지 않았으면 null. */
  rateLimits: RateLimitView[] | null;
  /** 연결된 데몬이 범위별 속도 제한을 모름(옛 데몬). */
  rateLimitsUnsupported: boolean;
}

export interface Actions {
  boot(): Promise<void>;
  loadAll(): Promise<void>;
  call<T>(p: Promise<T>, opts?: { ok?: string; quiet?: boolean }): Promise<T | undefined>;
  toast(kind: ToastKind, text: string): void;
  dismissToast(id: number): void;
  openModal(m: Modal): void;
  /** 친구·초대 링크를 맞는 창에 채워 엽니다(웹 초대 페이지가 연 링크, 메시지의 링크). 앱 링크가 아니면 false. */
  openAppLink(url: string): boolean;
  closeModal(): void;
  setMobilePane(pane: MobilePane): void;
  /** 휴대폰의 뒤로 가기: 창을 닫거나 한 칸 뒤로 갑니다. 첫 화면이라 갈 곳이 없으면 false. */
  mobileBack(): boolean;
  refreshSupporter(): Promise<void>;
  /** 표시 상태·마지막 동기화 비공개를 바꿉니다(`presence.set`). 오프라인으로 들어가거나 나오면 데몬이 다시 뜹니다. */
  setPresence(p: PresenceSetParams): Promise<void>;
  /** 후원자 전용 기능을 열어도 되는지. 아니면 후원 확인 창을 엽니다. */
  supporterOk(): boolean;
  setPrefs(patch: Partial<Prefs>): void;
  setHomeTab(t: State['homeTab']): void;

  refreshSpaces(): Promise<void>;
  refreshSpace(space: SpaceId): void;
  ensureSpace(space: SpaceId): Promise<SpaceView | undefined>;
  refreshFriends(): Promise<void>;
  /** 내 신원을 다시 읽습니다(내 프로필이 다른 기기에서 왔을 때). */
  refreshIdentity(): Promise<void>;
  /** 임시 연락처에게 친구 추가(`friend.propose`). 상대가 수락하면 친구가 되고, 서로 보냈으면 바로 친구가 됩니다. */
  proposeFriend(user: UserId): Promise<void>;
  /** 그 연락처와의 1대1 대화를 엽니다(`friend.open`). 1대1을 크루로 바꿨으면 둘만의 1대1 대화가 새로 생깁니다. */
  openFriend(user: UserId): Promise<void>;
  refreshConfirmations(): Promise<void>;
  refreshBlocked(): Promise<void>;
  refreshRateLimits(): Promise<void>;
  /** 한 범위의 속도 제한을 통째로 바꿉니다(바이트/초, 둘 다 null이면 지움). 데몬에 반영됐으면 참. */
  setRateLimit(scope: RateScope, up: number | null, down: number | null): Promise<boolean>;
  isBlocked(user: UserId): boolean;
  blockUser(user: UserId, name: string): void;
  unblockUser(user: UserId): Promise<void>;
  /** 남의 계정에서 기기 하나만 차단(확인 창을 거침). `device`는 기기 이름. */
  blockDevice(user: UserId, deviceId: DeviceId, device: string): void;
  unblockDevice(user: UserId, deviceId: DeviceId): Promise<void>;
  /**
   * 다른 기기에서 이 기기를 폐기해 데몬이 로컬 데이터와 신원을 지웠을 때(device.wiped): 화면 메모리도 버리고 신원부터
   * 다시 읽어 온보딩으로 돌아갑니다(남은 대화 내용을 계속 보여 주지 않게).
   */
  resetAfterWipe(): Promise<void>;
  /** 이 기기의 계정과 데이터를 지웁니다(`identity.delete`). */
  deleteAccount(): Promise<void>;
  /**
   * 이 기기의 API 토큰(AI·자동화 포함)이 휘발성 채널을 읽게 할지(`channel.allow_api_read`, 사람만). 허용은 경고를 보여 주고
   * 확인받은 뒤에만, 다시 막기는 바로 합니다.
   */
  setApiReadAllowed(space: SpaceId, channel: ChannelId, allowed: boolean): void;
  /**
   * 채널을 P2P 전용으로 바꾸거나 Tor로 되돌립니다(`channel.set_p2p`, 관리자·사람만). 어느 쪽이든 무엇이 바뀌는지 확인받은
   * 뒤에만 보냅니다.
   */
  setChannelP2p(space: SpaceId, channel: ChannelId, enabled: boolean): void;
  /**
   * 익명 채널 켜기·끄기(`channel.update`의 `anonymous`, 채널 관리 권한). 통화 채널의 익명을 끄면 통화 중인 사람이 모두
   * 나가므로 확인받은 뒤에만 보냅니다.
   */
  setChannelAnonymous(space: SpaceId, channel: ChannelId, enabled: boolean): void;
  /** 사이드바에서 끌어 놓은 채널·카테고리를 옮깁니다(`channel.update`의 `position`·`parent`, 채널 관리 권한). */
  moveChannel(space: SpaceId, channel: ChannelId, target: ChannelId, where: DropWhere): Promise<void>;
  /** 이 기기로 P2P 전용 채널에 참가합니다(`channel.p2p_join`). 잠금 화면의 경고를 본 뒤 누르는 단추에서만 부릅니다. */
  joinP2p(space: SpaceId, channel: ChannelId): Promise<void>;
  /** P2P 전용 채널 참가를 그만둡니다(`channel.p2p_leave`). 이 기기에서 다시 잠기므로 화면의 내용·통화도 정리합니다. */
  leaveP2p(space: SpaceId, channel: ChannelId): Promise<void>;
  /**
   * 사람·크루의 신뢰 스위치(`trust.set_user`·`trust.set_space`, 사람만). 켤 때는 IP 노출 경고를 확인받은 뒤에만 보내고
   * (‘다시 보지 않기’를 고른 뒤로는 바로), 끄기는 바로 보냅니다. 데몬에 반영됐으면 참.
   */
  setTrust(target: TrustTarget, enabled: boolean): Promise<boolean>;
  /** 채널(없으면 그 공간의 안 읽은 채널 모두)을 읽음으로. 아직 받지 않은 새 글(`has_new`)은 열어야 받아 옵니다. */
  markRead(space: SpaceId, channel?: ChannelId): void;
  /** 신고하며 숨긴 메시지를 화면에서 뺍니다(데몬은 이후 목록에서 빼고 돌려줌, 따로 알림은 없음). */
  hideMessage(space: SpaceId, channel: ChannelId, id: EventId): void;
  /** 그 채널을 열고 메시지를 찾아 보여 줍니다. 포럼 게시글·댓글은 `post`(게시글 ID)를 줍니다. 검색 결과는 열어 둡니다. */
  jumpToMessage(space: SpaceId, channel: ChannelId, id: EventId, post?: EventId): Promise<void>;

  selectHome(): void;
  selectLibrary(): void;
  /** 이 기기에서 지운 첨부(`blob.evict`)를 화면의 메시지에서도 받지 않은 것으로 돌립니다. */
  markEvicted(blob: BlobId): void;
  selectSpace(space: SpaceId): Promise<void>;
  /** `keepSearch`면 검색 패널을 닫지 않습니다(검색 결과에서 옮길 때). */
  selectChannel(space: SpaceId, channel: ChannelId, keepSearch?: boolean): Promise<void>;
  openChannelLink(space: SpaceId, channel: ChannelId): void;
  setFocused(focused: boolean): void;

  loadLatest(space: SpaceId, channel: ChannelId, thread?: EventId): Promise<void>;
  loadOlder(space: SpaceId, channel: ChannelId, thread?: EventId): Promise<void>;
  fillGap(space: SpaceId, channel: ChannelId, gap: GapView): Promise<void>;
  send(
    space: SpaceId,
    channel: ChannelId,
    text: string,
    attachments: AttachmentInput[],
    opts?: { replyTo?: EventId; thread?: EventId },
  ): Promise<boolean>;
  setReply(key: string, m: MessageView | undefined): void;
  setEditing(e: State['editing']): void;
  edit(space: SpaceId, channel: ChannelId, id: EventId, text: string): Promise<boolean>;
  remove(space: SpaceId, channel: ChannelId, m: MessageView): void;
  toggleReaction(space: SpaceId, channel: ChannelId, m: MessageView, emoji: string): Promise<void>;
  /**
   * 포럼 새 글(`forum.create_post`, 첨부·대문 지정 포함). 올리면 글 목록을 바로 다시 읽게 하고, 대문으로 올렸으면 공간 보기도
   * 다시 읽습니다. 올린 글 ID, 실패하면 null.
   */
  createForumPost(
    space: SpaceId,
    channel: ChannelId,
    post: { title: string; text: string; tags: number[]; attachments?: AttachmentInput[]; featured?: boolean },
  ): Promise<EventId | null>;
  joinChannel(space: SpaceId, channel: ChannelId): Promise<void>;
  leaveChannel(space: SpaceId, channel: ChannelId): Promise<void>;
  runSearch(query: string): Promise<void>;
  /** 같은 검색의 다른 쪽(`offset`부터). */
  searchPage(offset: number): Promise<void>;
  /** 쪽 크기를 바꾸고 지금 보던 첫 글이 든 쪽을 다시 불러옵니다. */
  setSearchPageSize(size: number): Promise<void>;
  clearSearch(): void;

  joinVoice(space: SpaceId, channel: ChannelId): Promise<void>;
  leaveVoice(): Promise<void>;
  setMuted(m: boolean): Promise<void>;
  setDeafened(d: boolean): Promise<void>;
  startShare(hint: ContentHint): Promise<void>;
  stopShare(): Promise<void>;
  setContentHint(hint: ContentHint): void;
  watchScreen(device: string, watching: boolean): Promise<void>;
  /** 보고 있는 화면의 화질 층(SFU 통화에서만 효과). */
  setScreenLayer(device: string, layer: ScreenLayer): void;
  /** 보고 있는 화면의 소리 크기(%, 이 통화 동안만). */
  setScreenVolume(device: string, percent: number): void;
  voiceSession(): VoiceSession | null;
}

export type Store = State & Actions;

/** 화면에 동시에 들고 있는 채널 상태 수(메모리 상한). */
const MAX_LOADED_CHANNELS = 12;
/** 공간별 마지막 채널을 기억하는 공간 수(설정 파일 64KiB 제한 안에 들게). */
const MAX_LAST_CHANNELS = 200;
const PAGE = 100;

/** message.list 페이지를 화면 순서((lamport, id) 오름차순)로 맞춥니다. 데몬은 오래된 것부터 주지만 페이지 순서에 기대지 않고
 *  실시간 알림(upsertSorted)과 같은 기준으로 정렬합니다. */
const inViewOrder = (ms: MessageView[]): MessageView[] => ms.slice().sort(compareMsg);

let session: VoiceSession | null = null;
let toastSeq = 1;
let booted = false;
/** 진행 중인 전체 읽기(부팅과 데몬 재연결이 겹쳐도 한 번만). */
let loadingAll: Promise<void> | null = null;
/** 앱을 띄운 알림 링크. 데몬이 뜨기 전에 와서 채널을 읽을 수 없으므로 `loadAll`이 끝나고 엽니다. */
let pendingChannel: { space: SpaceId; channel: ChannelId } | null = null;
const spaceRefreshTimers = new Map<SpaceId, ReturnType<typeof setTimeout>>();
const loadedOrder: string[] = [];

export function isVolatile(c: ChannelView | undefined): boolean {
  return c?.settings.security === 'volatile';
}

function trustToast(target: TrustTarget, enabled: boolean): string {
  const { name } = target;
  if (target.type === 'user') {
    return enabled ? t('{name} 님을 신뢰합니다. 이 사람에게 이 기기의 IP가 보입니다.', { name }) : t('{name} 님의 신뢰를 껐습니다.', { name });
  }
  return enabled ? t('「{name}」 크루를 신뢰합니다. 멤버에게 이 기기의 IP가 보입니다.', { name }) : t('「{name}」 크루의 신뢰를 껐습니다.', { name });
}

/** P2P 전용으로 바꿀 때 확인 창 본문. 멤버마다 경고를 받아들여야 다시 쓸 수 있다는 것을 먼저 알립니다. */
export function p2pOnWarning(name: string): string {
  return [
    t('#{name} 채널을 P2P 전용으로 바꿉니다.', { name }),
    '',
    t('• 이 채널의 메시지·파일·통화는 Tor를 거치지 않고 참가한 기기끼리 직접 오갑니다. 참가자끼리는 서로 IP 주소가 보입니다.'),
    t('• 나를 포함한 멤버 모두가 각 기기에서 IP 노출 경고를 받아들이고 참가해야 이 채널을 다시 쓸 수 있습니다. 그 전까지는 채널이 잠깁니다.'),
    t('• 보관 노드는 이 채널을 보관하지 않습니다.'),
    '',
    t('언제든 다시 Tor로 되돌릴 수 있습니다.'),
  ].join('\n');
}

/** Tor로 되돌릴 때 확인 창 본문. 이미 보인 IP는 되돌릴 수 없다는 것을 숨기지 않습니다. */
export function p2pOffWarning(name: string): string {
  return [
    t('#{name} 채널을 Tor로 되돌립니다.', { name }),
    '',
    t('• 이제부터 이 채널의 기록은 Tor를 거쳐 모든 멤버에게 동기화됩니다.'),
    t('• 이미 다른 참가자에게 보인 IP 주소는 다시 숨길 수 없습니다.'),
  ].join('\n');
}

export function findChannel(views: Record<SpaceId, SpaceView>, space: SpaceId, channel: ChannelId | null | undefined) {
  if (!channel) return undefined;
  return views[space]?.channels.find((c) => c.channel === channel);
}

/** 공간을 열 때 처음 보여 줄 채널: 기억한 채널 → 첫 일반 텍스트 채널 → 아무 텍스트 채널. */
export function defaultChannel(view: SpaceView, remembered?: ChannelId): ChannelId | null {
  const ok = (c: ChannelView) => c.settings.kind !== 'category';
  if (remembered && view.channels.some((c) => c.channel === remembered && ok(c))) return remembered;
  const sorted = sortChannels(view.channels).filter(ok);
  return (
    sorted.find((c) => c.settings.kind === 'text' && c.settings.security === 'normal')?.channel ??
    sorted.find((c) => c.settings.kind === 'text')?.channel ??
    sorted[0]?.channel ??
    null
  );
}

/** 카테고리 순서 → 카테고리 안 위치 순서로 정렬(카테고리 없는 채널이 먼저). */
export function sortChannels(channels: ChannelView[]): ChannelView[] {
  const cats = channels.filter((c) => c.settings.kind === 'category').sort((a, b) => a.settings.position - b.settings.position);
  const catPos = new Map(cats.map((c, i) => [c.channel, i]));
  const rank = (c: ChannelView) => {
    if (c.settings.kind === 'category') return [catPos.get(c.channel) ?? 0, -1];
    const p = c.settings.parent ? catPos.get(c.settings.parent) : undefined;
    return [p === undefined ? -1 : p, c.settings.position];
  };
  return channels.slice().sort((a, b) => {
    const [a1, a2] = rank(a);
    const [b1, b2] = rank(b);
    return a1 - b1 || a2 - b2 || a.settings.name.localeCompare(b.settings.name);
  });
}

export const useStore = create<Store>()((set, get) => {
  // ───────────── 알림 묶음: 프레임마다 한 번 적용 ─────────────
  let queue: Notification[] = [];
  let scheduled = false;
  const flush = () => {
    scheduled = false;
    const batch = queue;
    queue = [];
    if (batch.length === 0) return;
    const { patch, effects } = applyBatch(get(), batch);
    if (Object.keys(patch).length) set(patch as Partial<Store>);
    if (patch.channels) scrubReplies();
    for (const e of effects) runEffect(e);
    if (batch.some((n) => n.type === 'device.wiped')) void get().resetAfterWipe();
  };

  const enqueue = (batch: Notification[]) => {
    // 상한: 한 프레임에 너무 많이 쌓이면 오래된 것을 버리고 다시 맞춥니다.
    queue.push(...batch.slice(0, 5000));
    if (queue.length > 20000) {
      queue = [];
      runEffect({ type: 'resync' });
      return;
    }
    if (scheduled) return;
    scheduled = true;
    // 창이 가려지거나 최소화되면 rAF가 멈추는데, WebView2는 최소화를 document.hidden으로 알리지 않을 때가 있습니다.
    // 그래서 타이머도 함께 걸어 먼저 오는 쪽이 비웁니다(뒤에 온 쪽은 빈 대기열이라 그냥 끝남). 안 그러면 다시 볼 때까지
    // 알림이 쌓여 연결이 끊기거나 느려진 것처럼 보입니다.
    const once = () => {
      if (scheduled) flush();
    };
    setTimeout(once, 250);
    if (typeof requestAnimationFrame === 'function' && !(typeof document !== 'undefined' && document.hidden)) requestAnimationFrame(once);
  };

  const runEffect = (e: Effect) => {
    const s = get();
    switch (e.type) {
      case 'refreshSpace':
        s.refreshSpace(e.space);
        break;
      case 'refreshSpaces':
        void s.refreshSpaces();
        break;
      case 'refreshFriends':
        void s.refreshFriends();
        break;
      case 'deviceLinkRequest':
        set({ deviceLinkRequest: e.request });
        break;
      case 'deviceLinkResult':
        set({ deviceLinkResult: e.status });
        break;
      case 'loadAll':
        void s.loadAll();
        break;
      case 'refreshIdentity':
        void s.refreshIdentity();
        break;
      case 'supporter':
        void s.refreshSupporter();
        break;
      case 'badges':
        set({ badgesTick: get().badgesTick + 1 });
        break;
      case 'toast':
        s.toast(e.kind, e.text);
        break;
      case 'markRead':
        void s.client?.call('channel.mark_read', { space: e.space, channel: e.channel, event: e.event }).catch(() => undefined);
        break;
      case 'voiceSignal':
        if (session && s.voice && s.voice.space === e.space && s.voice.channel === e.channel) {
          void session.handleSignal(e.from, e.call, e.payload).catch(() => undefined);
        }
        break;
      case 'voiceMedia':
        if (session && s.voice && s.voice.space === e.space && s.voice.channel === e.channel) {
          session.handleMedia(e.from, e.call, e.data);
        }
        break;
      case 'voiceRoster':
        if (session && s.voice && s.voice.space === e.space && s.voice.channel === e.channel) {
          if (endAnonCall({ roster: e.participants })) break;
          session.updateParticipants(e.participants, e.generation, e.mediaKey, {
            topology: e.topology,
            sfuHost: e.sfuHost,
            sfuHostUser: e.sfuHostUser,
          });
        }
        break;
      case 'resync': {
        void s.refreshSpaces();
        const sel = s.selected;
        if (sel.space !== 'home' && sel.channel) {
          s.refreshSpace(sel.space);
          void s.loadLatest(sel.space, sel.channel);
        }
        break;
      }
    }
  };

  /**
   * 익명으로 들어간 통화에서 익명이 꺼졌으면(데몬은 이미 스스로 나옴) 미디어를 멈추고 알립니다. 끊었으면 참.
   * 이름이 보이는 채널에 익명 참가자로 남지 않게 하려는 것입니다.
   */
  const endAnonCall = (next: { channel?: ChannelView; roster?: VoiceParticipant[] }): boolean => {
    if (!anonCallEnded(get().voice, next)) return false;
    void get().leaveVoice();
    get().toast('warn', ANON_CALL_ENDED);
    return true;
  };

  /** 신원·공간·친구·확인 대기·네트워크를 전부 읽습니다(loadAll이 한 번에 하나만 돌림). */
  const loadAllOnce = async () => {
    const client = get().client;
    if (!client) return;
    let identity: IdentityView;
    try {
      identity = await client.call('identity.get', {});
    } catch (e) {
      if (e instanceof RpcError && e.code === ErrorCode.NoIdentity) {
        set({ phase: 'onboarding' });
        return;
      }
      if (e instanceof RpcError && e.code === ErrorCode.DaemonUnavailable) {
        // 연결되면 onDaemonState에서 다시 부릅니다.
        set({ phase: 'loading' });
        return;
      }
      get().toast('error', t('데몬에서 신원을 읽지 못했습니다: {message}', { message: (e as Error).message }));
      return;
    }
    set({ identity, me: identity.user, myDevice: identity.device, phase: 'ready' });
    await Promise.all([
      get().refreshSpaces(),
      get().refreshFriends(),
      get().refreshConfirmations(),
      get().refreshBlocked(),
    ]);
    // 앱을 다시 띄워도 기다리는 새 기기의 허용 창을 잃지 않습니다.
    set({ deviceLinkRequest: await client.call('device.link.pending', {}).catch(() => null) });
    void get().refreshSupporter();
    const net = await client.call('net.status', {}).catch(() => null);
    if (net) set({ net });
    set({ presence: await client.call('presence.get', {}).catch(() => null) });
    const flair = await client.call('flair.list', {}).catch(() => []);
    set({ flair: Object.fromEntries(flair.map((f) => [f.user, f])) });
    // 처음 값만 읽고 그 뒤로는 transfer.rates 알림을 따릅니다(옛 데몬이면 표시하지 않음).
    const rates = await client.call('transfer.rates', {}).catch(() => null);
    if (rates) set({ rates });
    // 크루 보기는 미리 읽어 둡니다(레일 배지·커스텀 이모지).
    for (const s of get().spaces) void get().ensureSpace(s.space);
    if (pendingChannel) {
      const { space, channel } = pendingChannel;
      pendingChannel = null;
      await get().selectChannel(space, channel);
    }
    const sel = get().selected;
    if (sel.space !== 'home' && !get().spaces.some((s) => s.space === sel.space)) get().selectHome();
  };

  /** 차단이 바뀌면 멤버 표시(blocked)와 메시지 표시(author_blocked)를 데몬에서 다시 읽습니다. */
  const refreshAllViews = () => {
    for (const space of Object.keys(get().views)) get().refreshSpace(space);
    const { active } = get();
    if (active) {
      const [space, channel, thread] = active.split('/');
      void get().loadLatest(space, channel, thread);
    }
  };

  const savePrefs = () => {
    const c = get().client;
    if (c) void c.transport.savePrefs(get().prefs as unknown as Record<string, unknown>).catch(() => undefined);
  };

  const patchChannel = (key: string, fn: (c: ChannelMsgs) => ChannelMsgs) => {
    const cur = get().channels[key] ?? emptyChannel();
    set({ channels: { ...get().channels, [key]: fn(cur) } });
  };

  const touchLoaded = (key: string) => {
    const i = loadedOrder.indexOf(key);
    if (i >= 0) loadedOrder.splice(i, 1);
    loadedOrder.push(key);
    // 오래 안 본 채널 상태는 버립니다(다시 열면 데몬에서 읽음).
    while (loadedOrder.length > MAX_LOADED_CHANNELS) {
      const old = loadedOrder.shift();
      if (old && old !== get().active) {
        const copy = { ...get().channels };
        delete copy[old];
        set({ channels: copy });
        scrubReplies();
      }
    }
  };

  /** 답장 미리보기로 잡아 둔 휘발성 메시지 사본도 채널 내용과 함께 버립니다. */
  const scrubReplies = () => {
    const r = pruneReplyTo(get().replyTo, get().channels, Date.now());
    if (r) set({ replyTo: r });
  };

  /** 다른 곳으로 이동할 때 휘발성 채널 내용은 UI 메모리에서 바로 버립니다. */
  const dropVolatile = (exceptKey: string | null) => {
    const { channels, views } = get();
    let copy: Record<string, ChannelMsgs> | null = null;
    for (const key of Object.keys(channels)) {
      if (key === exceptKey) continue;
      const [space, channel] = key.split('/');
      if (!isVolatile(findChannel(views, space, channel))) continue;
      copy ??= { ...channels };
      delete copy[key];
      const i = loadedOrder.indexOf(key);
      if (i >= 0) loadedOrder.splice(i, 1);
    }
    if (copy) set({ channels: copy });
    scrubReplies();
  };

  /** 휘발성 채널을 보는 동안은 늘 창 캡처를 막습니다(앱 정책이라 끌 수 없음). */
  const applyProtection = async (volatile: boolean) => {
    const { client, protection } = get();
    const want = volatile;
    if (!client || want === protection.requested) return;
    set({ protection: { ...protection, requested: want } });
    try {
      const supported = await client.transport.setContentProtection(want);
      set({ protection: { requested: want, supported } });
    } catch {
      set({ protection: { requested: want, supported: false } });
    }
  };

  /** 채널 전체(글타래 답글·포럼 글 포함)를 읽음으로: 데몬이 그 채널의 가장 최근 메시지까지 읽음 위치를 옮깁니다. */
  const markChannelRead = (space: SpaceId, channel: ChannelId) => {
    void get().client?.call('channel.mark_read', { space, channel }).catch(() => undefined);
    clearUnread(space, channel);
  };

  const clearUnread = (space: SpaceId, channel: ChannelId) => {
    const { views, spaces } = get();
    const v = views[space];
    let removed = 0;
    if (v) {
      const idx = v.channels.findIndex((c) => c.channel === channel);
      if (idx >= 0 && v.channels[idx].unread > 0) {
        removed = v.channels[idx].unread;
        const copy = { ...v, channels: v.channels.slice() };
        copy.channels[idx] = { ...copy.channels[idx], unread: 0 };
        set({ views: { ...views, [space]: copy } });
      }
    }
    const sIdx = spaces.findIndex((x) => x.space === space);
    if (sIdx >= 0 && (removed > 0 || spaces[sIdx].mention > 0)) {
      const list = spaces.slice();
      const left = v ? v.channels.reduce((n, c) => n + (c.channel === channel ? 0 : c.unread), 0) : 0;
      list[sIdx] = { ...list[sIdx], unread: Math.max(0, left), mention: left > 0 ? list[sIdx].mention : 0 };
      set({ spaces: list });
    }
  };

  /**
   * 공간 보기가 바뀐 뒤 P2P 잠금을 맞춥니다. 이 기기에서 잠긴 채널(P2P로 바뀌었거나 참가를 그만둠)은 읽어 둔 내용을
   * 화면 메모리에서 버리고 통화도 끊습니다. 보고 있던 채널이 풀렸으면(다른 경로로 참가) 내용을 읽습니다.
   */
  const syncP2pLocks = (space: SpaceId, view: SpaceView) => {
    const locked = new Set(view.channels.filter(isP2pLocked).map((c) => c.channel));
    const { channels, voice, selected } = get();
    let copy: Record<string, ChannelMsgs> | null = null;
    for (const key of Object.keys(channels)) {
      const [sp, ch] = key.split('/');
      if (sp !== space || !locked.has(ch)) continue;
      copy ??= { ...channels };
      delete copy[key];
      const i = loadedOrder.indexOf(key);
      if (i >= 0) loadedOrder.splice(i, 1);
    }
    if (copy) {
      set({ channels: copy });
      scrubReplies();
    }
    if (voice && voice.space === space && locked.has(voice.channel)) {
      void get().leaveVoice();
      get().toast('warn', t('P2P 전용으로 바뀐 채널이라 통화를 끊었습니다. 경고를 확인하고 참가하면 다시 들어갈 수 있습니다.'));
    }
    if (selected.space === space && selected.channel && !locked.has(selected.channel)) {
      const c = view.channels.find((x) => x.channel === selected.channel);
      const key = chKey(space, selected.channel);
      if (c?.p2p && c.settings.kind === 'text' && !get().channels[key]?.loaded) void get().loadLatest(space, selected.channel);
    }
  };

  // 휘발성 메시지 만료를 화면에서도 1초마다 확인합니다.
  if (typeof window !== 'undefined') {
    setInterval(() => {
      const now = Date.now();
      const pruned = pruneExpired(get().channels, now);
      if (pruned) set({ channels: pruned });
      const typing = pruneTyping(get().typing, now);
      if (typing) set({ typing });
      scrubReplies();
    }, 1000);
  }

  return {
    // ApplyState
    me: null,
    myDevice: null,
    active: null,
    focused: typeof document === 'undefined' ? true : document.hasFocus(),
    channels: {},
    spaces: [],
    views: {},
    friends: [],
    requests: [],
    confirmations: [],
    net: null,
    presence: null,
    flair: {},
    rates: null,
    blobProgress: {},
    voiceRosters: {},
    forumTick: 0,
    reportsNew: {},
    typing: {},
    // State
    client: null,
    phase: 'loading',
    firstSyncSkipped: false,
    deviceLinkRequest: null,
    deviceLinkResult: null,
    deviceLinkUrl: null,
    supporter: null,
    badgesTick: 0,
    rateLimits: null,
    rateLimitsUnsupported: false,
    daemon: { state: 'connecting' },
    identity: null,
    selected: { space: 'home', channel: null },
    library: false,
    homeTab: 'online',
    modal: null,
    mobilePane: 'list',
    toasts: [],
    prefs: DEFAULT_PREFS,
    replyTo: {},
    editing: null,
    voice: null,
    voiceJoining: null,
    voiceTick: 0,
    protection: { requested: false, supported: null },
    search: null,
    blocked: null,
    blockedDevices: null,
    jump: null,
    composerFocus: null,
    sent: null,

    async boot() {
      // StrictMode 개발 모드에서 두 번 불려도 구독은 한 번만
      if (booted) return;
      booted = true;
      const client = await getClient();
      set({ client });
      // OS 알림을 누르면 그 채널을 엽니다.
      client.transport.onOpenChannel?.(({ space, channel }) => void get().selectChannel(space, channel));
      // 웹 초대 페이지의 '앱에서 열기'. 어느 사이트든 띄울 수 있으므로 창에 채우기만 하고, 참가는 사용자가 누릅니다.
      client.transport.onOpenLink?.((url) => {
        const target = channelLink(url);
        if (target) get().openChannelLink(target.space, target.channel);
        else get().openAppLink(url);
      });
      client.transport.onDaemonState((d) => {
        const prev = get().daemon.state;
        set({ daemon: d });
        // 데몬이 (다시) 붙으면 전부 새로 읽습니다. 연결 전에 실패해 로딩 화면에 머문 경우도 여기서 풀립니다.
        if (d.state === 'connected' && prev !== 'connected') void get().loadAll();
      });
      client.subscribe(enqueue);
      try {
        const raw = await client.transport.loadPrefs();
        set({ prefs: { ...DEFAULT_PREFS, ...sanitizePrefs(raw) } });
      } catch {
        // 기본값 사용
      }
      await get().loadAll();
    },

    loadAll() {
      if (!loadingAll) {
        loadingAll = loadAllOnce().finally(() => {
          loadingAll = null;
        });
      }
      return loadingAll;
    },

    async call(p, opts) {
      try {
        const r = await p;
        if (opts?.ok) get().toast('success', opts.ok);
        return r;
      } catch (e) {
        if (!opts?.quiet) {
          const err = e instanceof RpcError ? e : new RpcError(ErrorCode.Internal, String(e));
          if (err.code === ErrorCode.ConfirmationRequired) get().toast('warn', t('사람 확인을 기다리는 작업입니다(설정 → 확인 대기).'));
          else get().toast('error', err.message);
        }
        return undefined;
      }
    },

    toast(kind, text) {
      const id = toastSeq++;
      set({ toasts: [...get().toasts.slice(-4), { id, kind, text }] });
      setTimeout(() => get().dismissToast(id), kind === 'error' ? 8000 : 5000);
    },
    dismissToast(id) {
      set({ toasts: get().toasts.filter((x) => x.id !== id) });
    },
    openAppLink(url) {
      const kind = appLinkKind(url);
      if (kind === 'friend') get().openModal({ kind: 'addFriend', url });
      else if (kind === 'invite') get().openModal({ kind: 'joinCrew', url });
      else if (kind === 'archive') get().toast('info', t('보관 노드 링크는 크루 설정 › 보관 노드에 붙여 넣으세요'));
      else if (kind === 'device') {
        // 시작 중에 온 링크도 받아 둡니다. 신원이 없으면 연결 화면이 칸에 채웁니다.
        if (get().phase === 'ready') get().toast('info', t('기기 연결 링크는 계정이 없는 새 기기에서 여세요'));
        else set({ deviceLinkUrl: url });
      }
      return kind !== null;
    },
    async refreshSupporter() {
      const supporter = await get().client?.call('supporter.status', {}).catch(() => null);
      if (supporter === undefined) return;
      set({ supporter });
      // 구독이 끝나 잠기면 열려 있던 라이브러리·뷰어도 닫습니다.
      if (supporter && !supporter.active) {
        if (get().library) get().selectHome();
        const kind = get().modal?.kind;
        if (kind === 'viewer' || kind === 'fileViewer') get().closeModal();
      }
    },

    async setPresence(p) {
      const client = get().client;
      if (!client) return;
      const v = await get().call(client.call('presence.set', p));
      if (v) set({ presence: v });
    },

    supporterOk() {
      if (get().supporter?.active !== false) return true;
      get().openModal({ kind: 'supporter' });
      return false;
    },

    openModal(m) {
      if ((m.kind === 'viewer' || m.kind === 'fileViewer') && !get().supporterOk()) return;
      if (m.kind === 'confirm' && m.back === undefined) {
        const cur = get().modal;
        // 확인 창 위에 확인 창은 겹치지 않고, 그 아래 창으로 돌아갈 곳을 물려받습니다.
        const back = cur?.kind === 'confirm' || cur?.kind === 'trustWarning' ? cur.back : cur;
        m = { ...m, back: back ?? null };
      }
      const cur = get().modal;
      set({ modal: m });
      // 화면 고르기를 끝내지 않고 다른 창으로 바꾸면 취소로 알립니다(기다리는 쪽이 멈추지 않게).
      if (cur?.kind === 'screenSource' && cur !== m) cur.onCancel();
    },
    closeModal() {
      const cur = get().modal;
      set({ modal: null });
      if (cur?.kind === 'screenSource') cur.onCancel();
    },
    setPrefs(patch) {
      set({ prefs: { ...get().prefs, ...patch } });
      // 통화 중에 바꾼 음성·화면 설정은 바로 걸어 둡니다.
      if (patch.media) session?.setMedia(patch.media);
      savePrefs();
    },
    setHomeTab(t) {
      set({ homeTab: t });
    },

    async refreshSpaces() {
      const client = get().client;
      if (!client) return;
      const spaces = await client.call('space.list', {}).catch(() => null);
      if (spaces) set({ spaces });
    },

    refreshSpace(space) {
      // 같은 공간 갱신 알림이 몰려도 한 번만 읽습니다.
      if (spaceRefreshTimers.has(space)) return;
      spaceRefreshTimers.set(
        space,
        setTimeout(async () => {
          spaceRefreshTimers.delete(space);
          const client = get().client;
          if (!client) return;
          try {
            const view = await client.call('space.get', { space });
            set({ views: { ...get().views, [space]: view } });
            syncP2pLocks(space, view);
            const voice = get().voice;
            if (voice?.space === space) {
              const c = view.channels.find((x) => x.channel === voice.channel);
              if (c) endAnonCall({ channel: c });
            }
            const sel = get().selected;
            if (sel.space === space && sel.channel && !view.channels.some((c) => c.channel === sel.channel)) {
              // 보던 채널이 지워졌거나 권한이 바뀜
              set({ selected: { space, channel: defaultChannel(view) } });
            }
            void get().refreshSpaces();
          } catch (e) {
            if (e instanceof RpcError && e.code === ErrorCode.NotFound) {
              const views = { ...get().views };
              delete views[space];
              set({ views, spaces: get().spaces.filter((s) => s.space !== space) });
              if (get().selected.space === space) get().selectHome();
            }
          }
        }, 80),
      );
    },

    async ensureSpace(space) {
      const have = get().views[space];
      if (have) return have;
      const client = get().client;
      if (!client) return undefined;
      const view = await client.call('space.get', { space }).catch(() => undefined);
      if (view) set({ views: { ...get().views, [space]: view } });
      return view;
    },

    async refreshIdentity() {
      const identity = await get().client?.call('identity.get', {}).catch(() => null);
      if (identity) set({ identity });
    },

    async refreshFriends() {
      const client = get().client;
      if (!client) return;
      const [friends, requests] = await Promise.all([
        client.call('friend.list', {}).catch(() => null),
        client.call('friend.requests', {}).catch(() => null),
      ]);
      if (friends) set({ friends });
      if (requests) set({ requests });
    },

    async proposeFriend(user) {
      const client = get().client;
      if (!client) return;
      const r = await get().call(client.call('friend.propose', { user }));
      if (!r) return;
      get().toast('success', r.status === 'friends' ? t('친구가 됐습니다.') : t('친구 요청을 보냈습니다. 상대가 수락하면 친구가 됩니다.'));
      await get().refreshFriends();
    },

    async openFriend(user) {
      const client = get().client;
      if (!client) return;
      const r = await get().call(client.call('friend.open', { user }));
      if (r) await get().selectSpace(r.space);
    },

    async refreshConfirmations() {
      const client = get().client;
      if (!client) return;
      const list = await client.call('confirm.list', {}).catch(() => null);
      if (list) set({ confirmations: list });
    },

    async refreshBlocked() {
      const client = get().client;
      if (!client) return;
      try {
        set({ blocked: await client.call('user.blocked', {}) });
      } catch (e) {
        // 모더레이션 전 데몬: 차단 목록 없이 동작합니다.
        if (e instanceof RpcError && e.code === ErrorCode.MethodNotFound) set({ blocked: null });
      }
      try {
        set({ blockedDevices: await client.call('user.blocked_devices', {}) });
      } catch (e) {
        if (e instanceof RpcError && e.code === ErrorCode.MethodNotFound) set({ blockedDevices: null });
      }
    },

    async refreshRateLimits() {
      const client = get().client;
      if (!client) return;
      try {
        set({ rateLimits: await client.call('rate.limits', {}), rateLimitsUnsupported: false });
      } catch (e) {
        if (e instanceof RpcError && e.code === ErrorCode.MethodNotFound) set({ rateLimits: [], rateLimitsUnsupported: true });
      }
    },

    async setRateLimit(scope, up, down) {
      const client = get().client;
      if (!client) return false;
      const r = await get().call(client.call('rate.set_limit', { scope, up_bps: up, down_bps: down }), {
        ok: up === null && down === null ? t('속도 제한을 지웠습니다') : t('속도 제한을 저장했습니다'),
      });
      if (r === undefined) return false;
      set({ rateLimits: withLimit(get().rateLimits ?? [], scope, up, down) });
      void get().refreshRateLimits();
      return true;
    },

    isBlocked(user) {
      return !!get().blocked?.some((b) => b.user === user);
    },

    blockUser(user, name) {
      const client = get().client;
      if (!client) return;
      get().openModal({
        kind: 'confirm',
        title: t('{name} 차단', { name }),
        body: t(
          '이 기기에서만 적용됩니다: 그 사람의 메시지를 접어 두고, 알림을 내지 않고, 친구 요청을 조용히 거절하며, 첨부를 자동으로 받지 않습니다. 상대에게 알리지 않으며, 같은 크루에 있는 다른 사람들에게는 여전히 보입니다.',
        ),
        danger: true,
        confirmLabel: tc('차단하기 단추', '차단'),
        onConfirm: async () => {
          const ok = await get().call(client.call('user.block', { user }), { ok: t('{name} 님을 차단했습니다', { name }) });
          if (ok !== undefined) {
            await get().refreshBlocked();
            refreshAllViews();
          }
        },
      });
    },

    async unblockUser(user) {
      const client = get().client;
      if (!client) return;
      const ok = await get().call(client.call('user.unblock', { user }), { ok: t('차단을 풀었습니다') });
      if (ok !== undefined) {
        await get().refreshBlocked();
        refreshAllViews();
      }
    },

    blockDevice(user, deviceId, device) {
      const client = get().client;
      if (!client) return;
      get().openModal({
        kind: 'confirm',
        title: t('「{device}」 기기 차단', { device }),
        body: t(
          '이 기기에서만 적용됩니다: 그 계정의 이 기기 하나만 연결을 거절하고, 그 기기가 쓴 메시지를 접어 두며, 알림과 첨부 자동 받기를 막습니다. 상대에게 알리지 않습니다. 그 기기가 이미 받은 대화나 다른 사람을 통해 받는 것은 막지 못하니, 수상한 기기라면 계정 주인에게 그 기기를 폐기하라고 알려 주세요.',
        ),
        danger: true,
        confirmLabel: t('기기 차단'),
        onConfirm: async () => {
          const ok = await get().call(client.call('user.block_device', { user, device: deviceId }), { ok: t('기기를 차단했습니다') });
          if (ok !== undefined) {
            await get().refreshBlocked();
            refreshAllViews();
          }
        },
      });
    },

    async unblockDevice(user, deviceId) {
      const client = get().client;
      if (!client) return;
      const ok = await get().call(client.call('user.unblock_device', { user, device: deviceId }), { ok: t('기기 차단을 풀었습니다') });
      if (ok !== undefined) {
        await get().refreshBlocked();
        refreshAllViews();
      }
    },

    async resetAfterWipe() {
      void get().leaveVoice();
      loadedOrder.length = 0;
      set({
        channels: {},
        views: {},
        spaces: [],
        friends: [],
        requests: [],
        confirmations: [],
        replyTo: {},
        search: null,
        editing: null,
        jump: null,
        composerFocus: null,
        sent: null,
        modal: null,
        identity: null,
        me: null,
        myDevice: null,
        firstSyncSkipped: false,
        deviceLinkRequest: null,
        deviceLinkResult: null,
        supporter: null,
        blocked: null,
        blockedDevices: null,
        rates: null,
        flair: {},
        rateLimits: null,
        selected: { space: 'home', channel: null },
        active: null,
        accountDeleting: false,
      });
      await get().loadAll();
    },

    async deleteAccount() {
      const { client, call } = get();
      if (!client) return;
      set({ accountDeleting: true });
      const ok = await call(client.call('identity.delete', {}), { ok: t('계정을 삭제했습니다') });
      if (ok === undefined) set({ accountDeleting: false });
    },

    setApiReadAllowed(space, channel, allowed) {
      const client = get().client;
      if (!client) return;
      const run = async () => {
        const r = await get().call(client.call('channel.allow_api_read', { space, channel, allowed }), {
          ok: allowed ? t('이 기기의 API 토큰이 이 채널을 읽을 수 있습니다') : t('API 토큰의 읽기를 다시 막았습니다'),
        });
        // 데몬은 이 설정에 공간 갱신 알림을 내지 않으므로 직접 다시 읽습니다(api_read_allowed).
        if (r !== undefined) get().refreshSpace(space);
      };
      if (!allowed) {
        void run();
        return;
      }
      get().openModal({
        kind: 'confirm',
        title: t('AI·자동화 읽기 허용'),
        body: [
          t('이 기기의 API 토큰(AI·자동화 포함)이 이 휘발성 채널의 메시지를 읽을 수 있게 됩니다.'),
          '',
          t('• AI가 읽는 순간 그쪽에 사실상 기록이 남을 수 있습니다. 수명이 지나도 거기서는 지워지지 않습니다.'),
          t('• 이 기기에만 적용됩니다. 다른 참가자는 이 허용을 알 수 없습니다.'),
          t('• 허용 표시(공간·채널 ID)만 이 기기 설정에 저장되고, 내용은 저장하지 않습니다.'),
          '',
          t('채널 위의 ‘AI 읽기 허용’ 표시를 누르면 언제든 다시 막을 수 있습니다.'),
        ].join('\n'),
        danger: true,
        confirmLabel: t('허용'),
        onConfirm: run,
      });
    },

    setChannelP2p(space, channel, enabled) {
      const client = get().client;
      if (!client) return;
      const name = findChannel(get().views, space, channel)?.settings.name ?? '';
      get().openModal({
        kind: 'confirm',
        title: enabled ? t('P2P 전용으로 바꾸기') : t('Tor로 되돌리기'),
        body: enabled ? p2pOnWarning(name) : p2pOffWarning(name),
        danger: enabled,
        confirmLabel: enabled ? t('P2P 전용으로 바꾸기') : t('Tor로 되돌리기'),
        onConfirm: async () => {
          const r = await get().call(client.call('channel.set_p2p', { space, channel, enabled }), {
            ok: enabled ? t('P2P 전용 채널로 바꿨습니다') : t('채널을 Tor로 되돌렸습니다'),
          });
          if (r !== undefined) get().refreshSpace(space);
        },
      });
    },

    async moveChannel(space, channel, target, where) {
      const client = get().client;
      const view = get().views[space];
      if (!client || !view) return;
      const moves = planChannelMove(sortChannels(view.channels), channel, target, where);
      // 한 채널씩 보내고 처음 실패에서 멈춥니다(오류 알림은 call이 띄움).
      for (const m of moves) {
        if ((await get().call(client.call('channel.update', { space, ...m }))) === undefined) break;
      }
      if (moves.length) get().refreshSpace(space);
    },

    setChannelAnonymous(space, channel, enabled) {
      const client = get().client;
      if (!client) return;
      const c = findChannel(get().views, space, channel);
      const run = async () => {
        const r = await get().call(client.call('channel.update', { space, channel, anonymous: enabled }), {
          ok: enabled ? t('익명 채널로 바꿨습니다') : t('익명을 껐습니다. 켜져 있는 동안 쓴 글은 익명으로 남습니다.'),
        });
        if (r !== undefined) get().refreshSpace(space);
      };
      if (enabled || c?.settings.kind !== 'voice') {
        void run();
        return;
      }
      get().openModal({
        kind: 'confirm',
        title: t('익명 끄기'),
        body: ANON_VOICE_OFF_WARNING,
        danger: true,
        confirmLabel: t('익명 끄기'),
        onConfirm: run,
      });
    },

    async joinP2p(space, channel) {
      const client = get().client;
      if (!client) return;
      const ok = await get().call(client.call('channel.p2p_join', { space, channel }));
      if (ok === undefined) return;
      const v = await client.call('space.get', { space }).catch(() => null);
      if (!v) return;
      set({ views: { ...get().views, [space]: v } });
      const c = v.channels.find((x) => x.channel === channel);
      if (c && !isP2pLocked(c) && c.settings.kind === 'text' && get().selected.channel === channel) await get().selectChannel(space, channel);
    },

    async leaveP2p(space, channel) {
      const client = get().client;
      if (!client) return;
      const ok = await get().call(client.call('channel.p2p_leave', { space, channel }), {
        ok: t('P2P 참여를 그만뒀습니다. 이 기기에서 채널이 다시 잠깁니다.'),
      });
      if (ok === undefined) return;
      const v = await client.call('space.get', { space }).catch(() => null);
      if (!v) return;
      set({ views: { ...get().views, [space]: v } });
      syncP2pLocks(space, v);
    },

    setTrust(target, enabled) {
      const client = get().client;
      if (!client) return Promise.resolve(false);
      const run = async () => {
        const r = await get().call(
          target.type === 'user'
            ? client.call('trust.set_user', { user: target.user, enabled })
            : client.call('trust.set_space', { space: target.space, enabled }),
          { ok: trustToast(target, enabled) },
        );
        if (r === undefined) return false;
        if (target.type === 'space') {
          // 스위치가 바로 따라오게 보기에 먼저 반영하고, 데몬 값으로 다시 맞춥니다(space.updated와 묶여 한 번만 읽음).
          const v = get().views[target.space];
          if (v) set({ views: { ...get().views, [target.space]: { ...v, trusted: enabled } } });
          get().refreshSpace(target.space);
        }
        return true;
      };
      if (!enabled || get().prefs.skipTrustWarning) return run();
      return new Promise<boolean>((resolve) => {
        const cur = get().modal;
        // 경고 위에 경고를 겹치지 않습니다: 떠 있던 경고는 취소로 보고 그 아래 창으로 돌아갈 곳을 물려받습니다.
        if (cur?.kind === 'trustWarning') cur.onCancel();
        const back = cur?.kind === 'trustWarning' ? cur.back : (cur ?? undefined);
        get().openModal({
          kind: 'trustWarning',
          target,
          back,
          onConfirm: async (skip) => {
            if (skip) get().setPrefs({ skipTrustWarning: true });
            resolve(await run());
          },
          onCancel: () => resolve(false),
        });
      });
    },

    markRead(space, channel) {
      const list = channel ? [channel] : (get().views[space]?.channels ?? []).filter((c) => c.unread > 0).map((c) => c.channel);
      for (const c of list) markChannelRead(space, c);
    },

    hideMessage(space, channel, id) {
      const prefix = chKey(space, channel);
      const channels = { ...get().channels };
      let changed = false;
      for (const [k, c] of Object.entries(channels)) {
        if ((k === prefix || k.startsWith(prefix + '/')) && c.messages.some((m) => m.id === id)) {
          channels[k] = { ...c, messages: c.messages.filter((m) => m.id !== id) };
          changed = true;
        }
      }
      if (changed) set({ channels });
    },

    async jumpToMessage(space, channel, id, post) {
      // 검색 패널을 닫지 않고 옮깁니다(닫았다 다시 열면 결과 목록의 스크롤이 맨 위로 돌아감).
      await get().selectChannel(space, channel, true);
      set({ jump: { key: chKey(space, channel), id, ...(post ? { post } : {}) } });
    },

    setMobilePane(mobilePane) {
      set({ mobilePane });
    },

    mobileBack() {
      const { modal, mobilePane, selected, library } = get();
      if (modal) {
        get().closeModal();
        return true;
      }
      // 라이브러리는 파일 → 폴더 목록 → 홈 순서로 돌아갑니다.
      if (library) {
        if (mobilePane === 'list') get().selectHome();
        else set({ mobilePane: 'list' });
        return true;
      }
      if (mobilePane === 'list') return false;
      if (mobilePane === 'members') {
        set({ mobilePane: 'chat' });
        return true;
      }
      const dm = selected.space !== 'home' && get().views[selected.space]?.kind === 'space';
      if (dm) {
        get().selectHome();
        return true;
      }
      // 채널을 벗어나면 읽음 처리와 휘발성 보호도 끝납니다(selectHome과 같게). 고른 채널은 목록에 표시되게 둡니다.
      set({ mobilePane: 'list', active: null });
      dropVolatile(null);
      void applyProtection(false);
      return true;
    },

    selectHome() {
      set({ selected: { space: 'home', channel: null }, library: false, active: null, search: null, mobilePane: 'list' });
      dropVolatile(null);
      void applyProtection(false);
    },

    selectLibrary() {
      if (!get().supporterOk()) return;
      get().selectHome();
      set({ library: true, mobilePane: 'chat' });
    },

    markEvicted(blob) {
      set((s) => {
        const { [blob]: _, ...blobProgress } = s.blobProgress;
        const channels = { ...s.channels };
        for (const [key, c] of Object.entries(s.channels)) {
          if (!c.messages.some((m) => m.attachments.some((a) => a.blob === blob))) continue;
          const messages = c.messages.map((m) => (m.attachments.some((a) => a.blob === blob) ? { ...m, attachments: m.attachments.map((a) => (a.blob === blob ? { ...a, downloaded: false } : a)) } : m));
          channels[key] = { ...c, messages };
        }
        return { blobProgress, channels };
      });
    },

    async selectSpace(space) {
      const view = await get().ensureSpace(space);
      if (!view) return;
      const ch = defaultChannel(view, get().prefs.lastChannel[space]);
      if (ch) await get().selectChannel(space, ch);
      else set({ selected: { space, channel: null }, library: false, active: null });
      // 1대1은 채널 목록이 없어 바로 대화로, 크루는 채널을 고르게 목록으로 갑니다.
      set({ mobilePane: view.kind === 'space' ? 'chat' : 'list' });
    },

    async selectChannel(space, channel, keepSearch = false) {
      const view = get().views[space] ?? (await get().ensureSpace(space));
      const c = view?.channels.find((x) => x.channel === channel);
      const key = chKey(space, channel);
      set({ selected: { space, channel }, library: false, active: key, editing: null, mobilePane: 'chat', ...(keepSearch ? {} : { search: null }) });
      dropVolatile(key);
      const volatile = isVolatile(c);
      void applyProtection(volatile);
      if (!volatile && c && c.settings.kind !== 'voice') {
        const prefs = get().prefs;
        if (prefs.lastChannel[space] !== channel) {
          set({ prefs: { ...prefs, lastChannel: rememberRecent(prefs.lastChannel, space, channel, MAX_LAST_CHANNELS) } });
          savePrefs();
        }
      }
      // 잠긴 P2P 전용 채널은 참가하기 전까지 내용을 읽지 않습니다(읽음 표시도 그대로).
      if (c && c.settings.kind === 'text' && !isP2pLocked(c)) {
        const cur = get().channels[key];
        if (!cur?.loaded) await get().loadLatest(space, channel);
        else touchLoaded(key);
        markChannelRead(space, channel);
      } else if (c && c.settings.kind === 'forum' && !isP2pLocked(c)) {
        markChannelRead(space, channel);
      }
    },

    openChannelLink(space, channel) {
      if (get().phase === 'ready') void get().selectChannel(space, channel);
      else pendingChannel = { space, channel };
    },

    setFocused(focused) {
      if (get().focused === focused) return;
      set({ focused });
      const { selected } = get();
      if (!focused || selected.space === 'home' || !selected.channel) return;
      // 창이 뒤에 있는 동안 온 메시지는 읽음 처리하지 않았으므로 돌아오면 데몬에도 알립니다(다시 켜도 숫자가 남지 않게).
      const c = get().views[selected.space]?.channels.find((x) => x.channel === selected.channel);
      if (c && (c.settings.kind === 'text' || c.settings.kind === 'forum') && !isP2pLocked(c)) {
        markChannelRead(selected.space, selected.channel);
      } else {
        clearUnread(selected.space, selected.channel);
      }
    },

    async loadLatest(space, channel, thread) {
      const client = get().client;
      if (!client) return;
      const key = chKey(space, channel, thread);
      try {
        const r = await client.call('message.list', { space, channel, limit: PAGE, thread });
        const msgs = inViewOrder(r.messages);
        patchChannel(key, (cur) => ({
          ...cur,
          // 읽는 동안 도착한 알림과 합칩니다.
          messages: cur.loaded ? prependOlder(cur.messages, msgs) : msgs,
          gaps: mergeGaps(cur.loaded ? cur.gaps : [], r.gaps),
          reachedStart: r.messages.length < PAGE,
          loaded: true,
          error: undefined,
        }));
        touchLoaded(key);
      } catch (e) {
        patchChannel(key, (cur) => ({ ...cur, loaded: true, error: (e as Error).message }));
      }
    },

    async loadOlder(space, channel, thread) {
      const client = get().client;
      const key = chKey(space, channel, thread);
      const cur = get().channels[key];
      if (!client || !cur?.loaded || cur.loadingOlder || cur.reachedStart || cur.messages.length === 0) return;
      patchChannel(key, (c) => ({ ...c, loadingOlder: true }));
      try {
        const r = await client.call('message.list', { space, channel, thread, before: cur.messages[0].id, limit: PAGE });
        const older = inViewOrder(r.messages);
        patchChannel(key, (c) => ({
          ...c,
          messages: prependOlder(c.messages, older),
          gaps: mergeGaps(c.gaps, r.gaps),
          reachedStart: r.messages.length < PAGE,
          loadingOlder: false,
        }));
      } catch (e) {
        patchChannel(key, (c) => ({ ...c, loadingOlder: false }));
        get().toast('error', t('이전 메시지를 읽지 못했습니다: {message}', { message: (e as Error).message }));
      }
    },

    async fillGap(space, channel, gap) {
      const client = get().client;
      if (!client) return;
      const key = chKey(space, channel);
      const gk = gapKey(gap);
      if (get().channels[key]?.filling.includes(gk)) return;
      patchChannel(key, (c) => ({ ...c, filling: [...c.filling, gk] }));
      try {
        // 구간은 데몬의 따라잡기(온라인 시더·멤버에게서 GetEvents)가 채웁니다. 앱은 구간 뒤쪽(before)부터 거꾸로
        // 다시 읽어 채워진 메시지를 합치고, 아직 비어 있으면 알려 줍니다.
        const r = await client.call('message.list', { space, channel, before: gap.before, limit: 200 });
        const page = inViewOrder(r.messages);
        const stillOpen = r.gaps.some((g) => g.after === gap.after && g.before === gap.before);
        patchChannel(key, (c) => ({
          ...c,
          messages: prependOlder(c.messages, page),
          gaps: stillOpen ? c.gaps : c.gaps.filter((g) => gapKey(g) !== gk),
          filling: c.filling.filter((x) => x !== gk),
        }));
        if (stillOpen) get().toast('warn', t('지금 이 구간을 가진 시더가 온라인이 아닙니다. 나중에 다시 시도하세요.'));
      } catch (e) {
        patchChannel(key, (c) => ({ ...c, filling: c.filling.filter((x) => x !== gk) }));
        get().toast('error', t('기록을 받지 못했습니다: {message}', { message: (e as Error).message }));
      }
    },

    async send(space, channel, text, attachments, opts) {
      const client = get().client;
      if (!client) return false;
      const r = await get().call(
        client.call('message.send', {
          space,
          channel,
          text,
          attachments: attachments.length ? attachments : undefined,
          reply_to: opts?.replyTo,
          thread: opts?.thread,
        }),
      );
      if (!r) return false;
      // 내 메시지 알림이 오지 않는 데몬이면 최신 페이지를 다시 읽어 합칩니다.
      const key = chKey(space, channel, opts?.thread);
      set({ sent: { key, id: r.id, at: Date.now() } });
      setTimeout(() => {
        const cur = get().channels[key];
        if (cur?.loaded && !cur.messages.some((m) => m.id === r.id)) {
          void client
            .call('message.list', { space, channel, thread: opts?.thread, limit: 20 })
            .then((page) => patchChannel(key, (c) => ({ ...c, messages: prependOlder(c.messages, page.messages) })))
            .catch(() => undefined);
        }
      }, 600);
      return true;
    },

    setReply(key, m) {
      // 답장을 고르면 그 목록의 입력창이 바로 커서를 잡게 합니다.
      const focus = m ? { composerFocus: { key, n: (get().composerFocus?.n ?? 0) + 1 } } : {};
      set({ replyTo: { ...get().replyTo, [key]: m }, ...focus });
    },
    setEditing(e) {
      set({ editing: e });
    },

    async edit(space, channel, id, text) {
      const client = get().client;
      if (!client) return false;
      const ok = await get().call(client.call('message.edit', { space, channel, id, text }));
      if (ok) set({ editing: null });
      return !!ok;
    },

    remove(space, channel, m) {
      const client = get().client;
      if (!client) return;
      get().openModal({
        kind: 'confirm',
        title: t('메시지 삭제'),
        body: t('삭제 이벤트를 보내면 모든 참가자 화면에서 지워집니다. 다만 이미 받아 간 사본(수정된 클라이언트, 스크린샷, 백업)은 강제로 지울 수 없습니다.'),
        danger: true,
        confirmLabel: t('삭제'),
        onConfirm: async () => {
          const ok = await get().call(client.call('message.delete', { space, channel, id: m.id }));
          // 포럼 게시글이면 글 목록에서도 바로 빠지게 합니다(알림을 기다리지 않고).
          if (ok !== undefined && !m.thread && findChannel(get().views, space, channel)?.settings.kind === 'forum') {
            set({ forumTick: get().forumTick + 1 });
          }
        },
      });
    },

    async createForumPost(space, channel, post) {
      const client = get().client;
      if (!client) return null;
      const { attachments, featured, ...rest } = post;
      const params = { space, channel, ...rest, ...(attachments?.length ? { attachments } : {}), ...(featured ? { featured: true } : {}) };
      const r = await get().call(client.call('forum.create_post', params), { ok: t('글을 올렸습니다') });
      if (!r) return null;
      // 내 글 알림(message.new)이 늦거나 오지 않아도 다른 채널을 다녀오지 않고 바로 보이게 합니다.
      set({ forumTick: get().forumTick + 1 });
      if (featured) get().refreshSpace(space);
      return r.id;
    },

    async toggleReaction(space, channel, m, emoji) {
      const client = get().client;
      if (!client) return;
      const mine = m.reactions.find((r) => r.emoji === emoji)?.me;
      // 바로 반영(낙관적 갱신) → 데몬의 message.updated가 최종 값을 덮어씁니다.
      const key = chKey(space, channel, m.thread);
      const cur = get().channels[key];
      if (cur?.loaded) {
        const reactions = m.reactions.map((r) => ({ ...r }));
        const r = reactions.find((x) => x.emoji === emoji);
        if (mine && r) {
          r.me = false;
          r.count = Math.max(0, r.count - 1);
        } else if (r) {
          r.me = true;
          r.count++;
        } else reactions.push({ emoji, count: 1, me: true });
        patchChannel(key, (c) => ({ ...c, messages: upsertSorted(c.messages, { ...m, reactions: reactions.filter((x) => x.count > 0) }) }));
      }
      await get().call(client.call(mine ? 'message.unreact' : 'message.react', { space, channel, id: m.id, emoji }));
    },

    async joinChannel(space, channel) {
      const client = get().client;
      if (!client) return;
      const ok = await get().call(client.call('channel.join', { space, channel }));
      if (ok === undefined) return;
      const key = chKey(space, channel);
      const copy = { ...get().channels };
      delete copy[key];
      set({ channels: copy });
      const v = await client.call('space.get', { space }).catch(() => null);
      if (v) set({ views: { ...get().views, [space]: v } });
      await get().loadLatest(space, channel);
    },

    async leaveChannel(space, channel) {
      const client = get().client;
      if (!client) return;
      const key = chKey(space, channel);
      // 화면에서 먼저 지웁니다(데몬도 퇴장 시 즉시 지움).
      const copy = { ...get().channels };
      delete copy[key];
      set({ channels: copy });
      scrubReplies();
      await get().call(client.call('channel.leave', { space, channel }));
      const v = await client.call('space.get', { space }).catch(() => null);
      if (v) set({ views: { ...get().views, [space]: v } });
    },

    async runSearch(query) {
      const client = get().client;
      const sel = get().selected;
      const q = query.trim();
      searchSeq++;
      if (!client || !q) {
        set({ search: null });
        return;
      }
      const space = sel.space === 'home' ? undefined : sel.space;
      const p = parseSearch(q, space ? get().views[space]?.members ?? [] : []);
      if (p.unknown !== undefined) {
        set({ search: { query: q, results: [], loading: false, total: 0, offset: 0, error: t("'{name}'(이)라는 멤버를 찾지 못했습니다", { name: p.unknown }) } });
        return;
      }
      if (!p.query && !p.author) {
        set({ search: null });
        return;
      }
      const req = { ...(space ? { space } : {}), query: p.query.slice(0, 200), ...(p.author ? { author: p.author } : {}) };
      set({ search: { query: q, results: [], loading: true, author: p.authorName, total: 0, offset: 0, req } });
      await get().searchPage(0);
    },
    async searchPage(offset) {
      const client = get().client;
      const cur = get().search;
      if (!client || !cur?.req) return;
      // 늦게 온 이전 응답이 새 검색·다른 쪽을 덮지 않게 차례 번호로 거릅니다. 기다리는 동안 이전 쪽은 그대로 둡니다.
      const seq = ++searchSeq;
      set({ search: { ...cur, loading: true, error: undefined } });
      try {
        const page = await client.call('message.search', { ...cur.req, offset: Math.max(0, offset), limit: get().prefs.searchPageSize });
        const now = get().search;
        if (seq !== searchSeq || !now) return;
        set({ search: { ...now, results: page.messages, total: page.total, offset: page.offset, tookMs: page.took_ms, loading: false } });
      } catch (e) {
        const now = get().search;
        if (seq !== searchSeq || !now) return;
        set({ search: { ...now, loading: false, error: t('검색하지 못했습니다: {message}', { message: e instanceof Error ? e.message : String(e) }) } });
      }
    },
    async setSearchPageSize(size) {
      const n = Math.round(size);
      if (!Number.isFinite(n)) return;
      const next = Math.min(SEARCH_PAGE_MAX, Math.max(SEARCH_PAGE_MIN, n));
      if (next === get().prefs.searchPageSize) return;
      get().setPrefs({ searchPageSize: next });
      const cur = get().search;
      if (cur?.req) await get().searchPage(Math.floor(cur.offset / next) * next);
    },
    clearSearch() {
      searchSeq++;
      set({ search: null });
    },

    // ───────────── 음성 ─────────────

    async joinVoice(space, channel) {
      const { client, identity, voice } = get();
      if (!client || !identity) return;
      // 화면 버튼과 별개로 여기서도 막습니다(Tor에서 WebRTC가 실제 IP를 드러내지 않게, 잠긴 P2P 채널에 들어가지 않게).
      const view = findChannel(get().views, space, channel);
      if (isP2pLocked(view)) {
        get().toast('warn', t('P2P 전용 채널입니다. 경고를 확인하고 참가한 뒤에 통화할 수 있습니다.'));
        return;
      }
      // 참가한 P2P 전용 채널은 Tor여도 직접 연결(WebRTC)로, 그 밖의 채널은 Tor로 도는 기기면 Tor 통화(소리만, 데몬이 정함)로 합니다.
      if (voice) await get().leaveVoice();
      const key = chKey(space, channel);
      set({ voiceJoining: key });
      try {
        const join = await client.call('voice.join', { space, channel });
        const s = new VoiceSession({
          client,
          space,
          channel,
          myDevice: identity.device,
          myUser: identity.user,
          join,
          // ICE 서버는 데몬이 정합니다(join 결과의 STUN 서버).
          iceServers: join.ice_servers ?? [],
          simulated: client.kind === 'mock',
          media: get().prefs.media,
          pickScreenSource: () =>
            new Promise<ScreenSource | null>((resolve) => {
              get().openModal({
                kind: 'screenSource',
                onPick: (src) => {
                  resolve(src);
                  get().closeModal();
                },
                onCancel: () => resolve(null),
              });
            }),
          // 나간(또는 갈아 끼운) 세션이 정리하며 내는 변화는 받지 않습니다: 받으면 나간 뒤에도 통화 화면이 남았습니다.
          onChange: (snap) => {
            if (session === s) set({ voice: snap, voiceTick: get().voiceTick + 1 });
          },
        });
        session = s;
        set({ voice: s.snapshot });
        await s.start();
        // 마이크 권한을 받은 뒤라야 서비스에 마이크 종류를 더할 수 있습니다.
        client.transport?.setCallActive?.(true);
      } catch (e) {
        if (session) await session.leave().catch(() => undefined);
        session = null;
        set({ voice: null });
        get().toast('error', t('통화에 참가하지 못했습니다: {message}', { message: (e as Error).message }));
      } finally {
        set({ voiceJoining: null });
      }
    },

    async leaveVoice() {
      const s = session;
      session = null;
      set({ voice: null });
      stopFakeScreens();
      get().client?.transport?.setCallActive?.(false);
      if (s) await s.leave().catch(() => undefined);
    },

    async setMuted(m) {
      await session?.setMuted(m);
    },
    async setDeafened(d) {
      await session?.setDeafened(d);
    },
    async startShare(hint) {
      if (!session) return;
      const s = session;
      const start = async (torScreen = false) => {
        try {
          await s.startScreenShare(hint, torScreen);
        } catch (e) {
          if ((e as Error).name !== 'NotAllowedError') get().toast('error', t('화면공유를 시작하지 못했습니다: {message}', { message: (e as Error).message }));
        }
      };
      // Tor로 잇는 상대에게 보내는 화면은 비상용이라 경고를 확인받습니다(서로 신뢰한 상대만 있으면 메시처럼).
      if (needsTorScreen(s.snapshot)) {
        get().openModal({
          kind: 'confirm',
          title: t('Tor로 화면공유(비상용)'),
          body: TOR_SCREEN_WARNING,
          confirmLabel: t('그래도 공유'),
          onConfirm: () => start(true),
        });
        return;
      }
      await start();
    },
    async stopShare() {
      await session?.stopScreenShare();
    },
    setContentHint(hint) {
      session?.setContentHint(hint);
    },
    async watchScreen(device, watching) {
      await session?.watch(device, watching);
    },
    setScreenLayer(device, layer) {
      session?.setScreenLayer(device, layer);
    },
    setScreenVolume(device, percent) {
      session?.setScreenVolume(device, percent);
    },
    voiceSession() {
      return session;
    },
  };
});

/** 저장된 UI 설정에서 아는 키·형식만 받아들입니다(파일이 손상돼도 앱이 뜨게). */
export function sanitizePrefs(raw: Record<string, unknown>): Partial<Prefs> {
  const out: Partial<Prefs> = {};
  if (raw.theme === 'dark' || raw.theme === 'light') out.theme = raw.theme;
  if (typeof raw.memberList === 'boolean') out.memberList = raw.memberList;
  if (typeof raw.bannerCollapsed === 'boolean') out.bannerCollapsed = raw.bannerCollapsed;
  if (typeof raw.notify === 'boolean') out.notify = raw.notify;
  if (raw.notifyCrew === 'all' || raw.notifyCrew === 'mentions' || raw.notifyCrew === 'off') out.notifyCrew = raw.notifyCrew;
  if (typeof raw.notifyPreview === 'boolean') out.notifyPreview = raw.notifyPreview;
  if (raw.notifyRules !== undefined) out.notifyRules = sanitizeRules(raw.notifyRules);
  if (typeof raw.skipTrustWarning === 'boolean') out.skipTrustWarning = raw.skipTrustWarning;
  if (typeof raw.closeToTray === 'boolean') out.closeToTray = raw.closeToTray;
  if (typeof raw.stripMetadata === 'boolean') out.stripMetadata = raw.stripMetadata;
  if (isLang(raw.lang)) out.lang = raw.lang;
  if (typeof raw.autoStart === 'boolean') out.autoStart = raw.autoStart;
  if (typeof raw.autoStartToTray === 'boolean') out.autoStartToTray = raw.autoStartToTray;
  if (Array.isArray(raw.permTemplates)) out.permTemplates = sanitizeTemplates(raw.permTemplates);
  if (raw.forumView && typeof raw.forumView === 'object') {
    const fv: Record<string, ForumViewPrefs> = {};
    for (const [k, v] of Object.entries(raw.forumView as Record<string, unknown>).slice(-MAX_FORUM_VIEWS)) {
      if (k.length > 96 || !v || typeof v !== 'object') continue;
      const x = v as Record<string, unknown>;
      fv[k] = {
        sort: x.sort === 'created' ? 'created' : 'activity',
        order: x.order === 'asc' ? 'asc' : 'desc',
        view: x.view === 'gallery' ? 'gallery' : 'list',
        match: x.match === 'all' ? 'all' : 'any',
      };
    }
    out.forumView = fv;
  }
  if (raw.media && typeof raw.media === 'object') out.media = sanitizeMedia(raw.media);
  if (typeof raw.searchPageSize === 'number' && Number.isInteger(raw.searchPageSize)) {
    out.searchPageSize = Math.min(SEARCH_PAGE_MAX, Math.max(SEARCH_PAGE_MIN, raw.searchPageSize));
  }
  if (raw.viewer && typeof raw.viewer === 'object') out.viewer = sanitizeViewer(raw.viewer);
  if (raw.library && typeof raw.library === 'object') out.library = sanitizeLibrary(raw.library);
  if (raw.lastChannel && typeof raw.lastChannel === 'object') {
    const lc: Record<string, string> = {};
    // 뒤쪽이 최근 것(rememberRecent 순서)이라 뒤에서부터 남깁니다.
    for (const [k, v] of Object.entries(raw.lastChannel as Record<string, unknown>).slice(-MAX_LAST_CHANNELS)) {
      if (typeof v === 'string' && k.length <= 64 && v.length <= 64) lc[k] = v;
    }
    out.lastChannel = lc;
  }
  return out;
}

/** 지금 연 채널(`active`)을 OS 알림 쪽(Tauri 프로세스 메모리)에 알립니다. 창이 앞에 있는 동안 그 채널은 알리지 않습니다.
 *  `active`가 바뀌는 모든 길(초기화·웹뷰 새로 고침 포함)을 한곳에서 잡습니다. */
useStore.subscribe((s, p) => {
  if (s.active === p.active && s.client === p.client) return;
  const [space, channel] = s.active?.split('/') ?? [];
  // 시험용 가짜 클라이언트에는 transport가 없을 수 있습니다.
  s.client?.transport?.setNotifyViewing?.(space ?? null, channel ?? null);
});

if (typeof window !== 'undefined') {
  window.addEventListener('focus', () => useStore.getState().setFocused(true));
  window.addEventListener('blur', () => useStore.getState().setFocused(false));
  window.addEventListener('beforeunload', () => void useStore.getState().leaveVoice());
}
