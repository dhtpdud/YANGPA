// 데몬 알림 묶음을 상태에 반영하는 순수 함수. 부수 효과(재조회·음성 협상·토스트)는 Effect로 돌려줍니다.
// 묶음 하나를 적용하는 동안 바뀐 맵만 한 번 복사합니다(10k 메시지 채널에서도 프레임당 한 번).

import type {
  ChannelId,
  DeviceId,
  DeviceLinkRequestView,
  DeviceLinkStatus,
  EventId,
  FlairView,
  FriendRequestView,
  FriendView,
  GapView,
  MessageView,
  NetStatus,
  Notification,
  PendingConfirmation,
  PresenceView,
  SignalPayload,
  SpaceId,
  SpaceSummary,
  SpaceView,
  Topology,
  TransferRates,
  UserId,
  VoiceParticipant,
} from '../api/types';
import { supporterErrorText } from '../supporter/supporter';
import { isMine, participantUsers } from '../lib/anon';
import { isP2pLocked } from '../lib/channel';
import { t } from '../i18n';
import { upsertSorted } from '../lib/rows';

export interface ChannelMsgs {
  /** (lamport, id) 오름차순 */
  messages: MessageView[];
  gaps: GapView[];
  reachedStart: boolean;
  loadingOlder: boolean;
  loaded: boolean;
  /** 채우는 중인 구간 키 */
  filling: string[];
  error?: string;
}

export interface VoiceRoster {
  participants: VoiceParticipant[];
  generation: number;
  topology: Topology;
  /** SFU 호스트 기기(SFU이고 정해졌을 때만). */
  sfuHost?: DeviceId;
  /** SFU 호스트 기기의 사용자. */
  sfuHostUser?: UserId;
}

export interface ApplyState {
  me: UserId | null;
  /** 이 기기 */
  myDevice?: DeviceId | null;
  /** 지금 보고 있는 채널 키 */
  active: string | null;
  focused: boolean;
  /** 사용자가 직접 계정을 지우는 중(`device.wiped`에 폐기 안내를 띄우지 않음) */
  accountDeleting?: boolean;
  channels: Record<string, ChannelMsgs>;
  spaces: SpaceSummary[];
  views: Record<SpaceId, SpaceView>;
  friends: FriendView[];
  requests: FriendRequestView[];
  confirmations: PendingConfirmation[];
  net: NetStatus | null;
  /** 이 기기의 표시 상태(`presence.get`). 모르는 데몬이면 null. */
  presence: PresenceView | null;
  /** 사용자마다 후원자 표시(`flair.list`·`flair` 알림). 없으면 아무것도 보이지 않음. */
  flair: Record<UserId, FlairView>;
  /** 지금 전체 올리기·받기 속도와 내가 내주는 첨부를 받는 사람(transfer.rates). 모르는 데몬이면 null. */
  rates: TransferRates | null;
  blobProgress: Record<string, { have: number; total: number }>;
  voiceRosters: Record<string, VoiceRoster>;
  /** 포럼 글 목록을 다시 불러올 신호(새 글·글 바뀜·새 답글) */
  forumTick: number;
  /** 공간별로 아직 보지 않은 새 신고 수(report.new). 신고 화면을 열면 0으로. */
  reportsNew: Record<SpaceId, number>;
  /** 채널(chKey)별로 지금 글을 쓰는 중인 사람(channel.typing). 몇 초 뒤 저절로 빠집니다. */
  typing: Record<string, TypingEntry[]>;
}

/** 글 쓰는 중인 사람 한 명. 익명 채널은 가명으로 가릅니다(user는 모두 익명 사용자). */
export interface TypingEntry {
  id: string;
  user: UserId;
  anonName?: string;
  until: number;
}

/** 마지막 알림 뒤 이만큼 보여 줍니다(보내는 쪽은 입력하는 동안 2.5초마다 알림). */
export const TYPING_MS = 6000;

const typingId = (user: UserId, anonName?: string) => (anonName ? `anon:${anonName}` : user);

/** 시간이 지난 '쓰는 중'을 뺍니다. 바뀐 것이 없으면 null. */
export function pruneTyping(typing: Record<string, TypingEntry[]>, now: number): Record<string, TypingEntry[]> | null {
  let out: Record<string, TypingEntry[]> | null = null;
  for (const [key, list] of Object.entries(typing)) {
    if (!list.some((x) => x.until <= now)) continue;
    out ??= { ...typing };
    const left = list.filter((x) => x.until > now);
    if (left.length) out[key] = left;
    else delete out[key];
  }
  return out;
}

export type ToastKind = 'info' | 'success' | 'warn' | 'error';

export type Effect =
  | { type: 'refreshSpace'; space: SpaceId }
  | { type: 'refreshSpaces' }
  | { type: 'refreshFriends' }
  | { type: 'deviceLinkRequest'; request: DeviceLinkRequestView }
  | { type: 'deviceLinkResult'; status: DeviceLinkStatus }
  | { type: 'loadAll' }
  | { type: 'refreshIdentity' }
  | { type: 'supporter' }
  | { type: 'badges' }
  | { type: 'voiceSignal'; space: SpaceId; channel: ChannelId; call: string; from: string; payload: SignalPayload }
  | { type: 'voiceMedia'; space: SpaceId; channel: ChannelId; call: string; from: string; data: string }
  | {
      type: 'voiceRoster';
      space: SpaceId;
      channel: ChannelId;
      participants: VoiceParticipant[];
      generation: number;
      mediaKey: string;
      topology: Topology;
      sfuHost?: DeviceId;
      sfuHostUser?: UserId;
    }
  | { type: 'toast'; kind: ToastKind; text: string }
  | { type: 'markRead'; space: SpaceId; channel: ChannelId; event?: EventId }
  | { type: 'resync' };

export function chKey(space: SpaceId, channel: ChannelId, thread?: EventId | null): string {
  return thread ? `${space}/${channel}/${thread}` : `${space}/${channel}`;
}

export function emptyChannel(): ChannelMsgs {
  return { messages: [], gaps: [], reachedStart: false, loadingOlder: false, loaded: false, filling: [] };
}

/** 나를 멘션한 글: 데몬이 정한 `mentions_me`(권한 있는 @everyone·@here 포함), 예전 데몬이면 글의 `<@내 ID>`. */
export function mentionsMe(m: Pick<MessageView, 'text' | 'mentions_me'>, user: UserId | null): boolean {
  return !!m.mentions_me || (!!user && m.text.includes(`<@${user}>`));
}

export function applyBatch(s: ApplyState, batch: Notification[]): { patch: Partial<ApplyState>; effects: Effect[] } {
  const effects: Effect[] = [];
  let channels: Record<string, ChannelMsgs> | null = null;
  let views: Record<SpaceId, SpaceView> | null = null;
  let spaces: SpaceSummary[] | null = null;
  let friends: FriendView[] | null = null;
  let requests: FriendRequestView[] | null = null;
  let confirmations: PendingConfirmation[] | null = null;
  let blobProgress: Record<string, { have: number; total: number }> | null = null;
  let voiceRosters: Record<string, VoiceRoster> | null = null;
  let net: NetStatus | null | undefined;
  let presence: PresenceView | undefined;
  let flair: Record<UserId, FlairView> | null = null;
  let rates: TransferRates | undefined;
  let reportsNew: Record<SpaceId, number> | null = null;
  let typing: Record<string, TypingEntry[]> | null = null;
  let forumTick = s.forumTick;
  const refresh = new Set<SpaceId>();
  let refreshFriends = false;
  let refreshSpaceList = false;

  const ch = () => (channels ??= { ...s.channels });
  const vw = () => (views ??= { ...s.views });
  const sp = () => (spaces ??= s.spaces.map((x) => ({ ...x })));
  const isForum = (space: SpaceId, channel: ChannelId) =>
    (views ?? s.views)[space]?.channels.find((c) => c.channel === channel)?.settings.kind === 'forum';

  /**
   * 메시지를 그 목록에 넣습니다. 포럼 게시글(글타래가 없는 포럼 메시지)은 자기 글타래의 첫 메시지이기도 해서, 열어 둔
   * 글타래에도 넣습니다(본문 수정·삭제가 바로 보이게). 새 글·글 변경이면 글 목록도 다시 읽게 합니다.
   */
  const upsert = (space: SpaceId, channel: ChannelId, m: MessageView) => {
    const post = !m.thread && isForum(space, channel);
    const keys = [chKey(space, channel, m.thread)];
    if (post) keys.push(chKey(space, channel, m.id));
    for (const key of keys) {
      const cur = (channels ?? s.channels)[key];
      if (cur?.loaded) ch()[key] = { ...cur, messages: upsertSorted(cur.messages, m) };
    }
    if (post) forumTick++;
  };

  /** 그 채널의 '쓰는 중' 목록에서 한 명을 빼거나(until 없음) 새로 적습니다. */
  const setTyping = (space: SpaceId, channel: ChannelId, id: string, entry?: TypingEntry) => {
    const key = chKey(space, channel);
    const cur = (typing ?? s.typing)[key] ?? [];
    if (!entry && !cur.some((x) => x.id === id)) return;
    const next = cur.filter((x) => x.id !== id);
    if (entry) next.push(entry);
    typing ??= { ...s.typing };
    if (next.length) typing[key] = next;
    else delete typing[key];
  };

  const bumpUnread = (space: SpaceId, channel: ChannelId, mention: boolean) => {
    const summary = sp().find((x) => x.space === space);
    if (summary) {
      summary.unread++;
      if (mention) summary.mention++;
    }
    const v = (views ?? s.views)[space];
    if (v) {
      const idx = v.channels.findIndex((c) => c.channel === channel);
      if (idx >= 0) {
        const copy = { ...v, channels: v.channels.slice() };
        copy.channels[idx] = { ...copy.channels[idx], unread: copy.channels[idx].unread + 1 };
        vw()[space] = copy;
      }
    }
  };

  for (const n of batch) {
    switch (n.type) {
      case 'message.new': {
        upsert(n.space, n.channel, n.message);
        // 글을 올렸으면 '쓰는 중'은 끝(다음 입력 알림이 오면 다시).
        setTyping(n.space, n.channel, typingId(n.message.author, n.message.anon_name));
        if (n.message.thread) forumTick++;
        // 익명 글은 글쓴이가 가려져 있어 내 글인지는 `mine`으로만 압니다.
        if (!isMine(n.message, s.me)) {
          // 잠긴 P2P 전용 채널은 열어 두어도 내용을 보이지 않으므로 읽음으로 치지 않습니다.
          const locked = isP2pLocked((views ?? s.views)[n.space]?.channels.find((c) => c.channel === n.channel));
          // 글타래 답글·포럼 글도 그 채널을 보고 있으면 읽은 것으로 칩니다(안 읽음 수는 채널 단위).
          const viewing = s.active === chKey(n.space, n.channel) && s.focused && !locked;
          if (viewing) effects.push({ type: 'markRead', space: n.space, channel: n.channel, event: n.message.id });
          else if (!n.message.thread) bumpUnread(n.space, n.channel, mentionsMe(n.message, s.me));
        }
        break;
      }
      case 'message.updated':
        upsert(n.space, n.channel, n.message);
        break;
      case 'message.expired': {
        const ids = new Set(n.ids);
        const prefix = chKey(n.space, n.channel);
        for (const [key, cur] of Object.entries(channels ?? s.channels)) {
          if (key !== prefix && !key.startsWith(prefix + '/')) continue;
          if (!cur.messages.some((m) => ids.has(m.id))) continue;
          ch()[key] = { ...cur, messages: cur.messages.filter((m) => !ids.has(m.id)) };
        }
        break;
      }
      case 'space.updated':
        refresh.add(n.space);
        break;
      case 'friend.request':
        requests ??= s.requests.slice();
        if (!requests.some((r) => r.request_id === n.request_id)) {
          requests.push({ request_id: n.request_id, user: n.user, profile: n.profile, received_at: Date.now() });
          effects.push({ type: 'toast', kind: 'info', text: t('{name} 님이 친구 요청을 보냈습니다', { name: n.profile.name || t('알 수 없음') }) });
        }
        break;
      case 'friend.accepted':
        effects.push({ type: 'refreshFriends' }, { type: 'refreshSpaces' });
        effects.push({ type: 'toast', kind: 'success', text: t('친구 연결이 끝났습니다') });
        break;
      case 'friend.updated':
        // 임시 연락처가 생기거나 만료되거나 친구가 됨: 종류와 1대1 대화 잠금이 바뀌므로 둘 다 다시 읽습니다(묶음마다 한 번).
        if (!refreshFriends) {
          refreshFriends = true;
          effects.push({ type: 'refreshFriends' });
        }
        if (!refreshSpaceList) {
          refreshSpaceList = true;
          effects.push({ type: 'refreshSpaces' });
        }
        break;
      case 'presence': {
        // 마지막 동기화 시각은 바뀐 때만 실려 옵니다(나 자신은 늘 없음).
        const shown = { online: n.online, away: !!n.away, ...(n.last_seen !== undefined && { last_seen: n.last_seen }) };
        const fIdx = (friends ?? s.friends).findIndex((f) => f.user === n.user);
        if (fIdx >= 0) {
          friends ??= s.friends.slice();
          friends[fIdx] = { ...friends[fIdx], ...shown };
        }
        for (const [id, v] of Object.entries(views ?? s.views)) {
          const mIdx = v.members.findIndex((m) => m.user === n.user);
          if (mIdx < 0) continue;
          const members = v.members.slice();
          members[mIdx] = { ...members[mIdx], ...shown };
          vw()[id] = { ...v, members };
        }
        break;
      }
      case 'presence.self': {
        const { type: _t, ...rest } = n;
        presence = rest;
        break;
      }
      case 'flair': {
        const { type: _t, ...rest } = n;
        flair ??= { ...s.flair };
        flair[n.user] = rest;
        break;
      }
      case 'channel.typing': {
        if (n.user === s.me && !n.anon_name) break;
        const id = typingId(n.user, n.anon_name);
        setTyping(n.space, n.channel, id, { id, user: n.user, anonName: n.anon_name, until: Date.now() + TYPING_MS });
        break;
      }
      case 'profile.updated': {
        // 내 프로필은 내 다른 기기에서 오기도 합니다.
        if (n.user === s.me && !effects.some((e) => e.type === 'refreshIdentity')) effects.push({ type: 'refreshIdentity' });
        // 이름·사진이 바뀐 친구·멤버의 프로필을 다시 읽습니다(묶음마다 친구 목록은 한 번).
        if (!refreshFriends && (friends ?? s.friends).some((f) => f.user === n.user)) {
          refreshFriends = true;
          effects.push({ type: 'refreshFriends' });
        }
        for (const [id, v] of Object.entries(views ?? s.views)) {
          if (v.members.some((m) => m.user === n.user)) refresh.add(id);
        }
        break;
      }
      case 'voice.signal':
        effects.push({ type: 'voiceSignal', space: n.space, channel: n.channel, call: n.call, from: n.from, payload: n.payload });
        break;
      case 'voice.media':
        effects.push({ type: 'voiceMedia', space: n.space, channel: n.channel, call: n.call, from: n.from, data: n.data });
        break;
      case 'voice.participants': {
        voiceRosters ??= { ...s.voiceRosters };
        voiceRosters[chKey(n.space, n.channel)] = {
          participants: n.participants,
          generation: n.generation,
          topology: n.topology,
          sfuHost: n.sfu_host,
          sfuHostUser: n.sfu_host_user,
        };
        const v = (views ?? s.views)[n.space];
        const idx = v?.channels.findIndex((c) => c.channel === n.channel) ?? -1;
        if (v && idx >= 0) {
          const copy = { ...v, channels: v.channels.slice() };
          copy.channels[idx] = { ...copy.channels[idx], participants: participantUsers(n.participants) };
          vw()[n.space] = copy;
        }
        effects.push({
          type: 'voiceRoster',
          space: n.space,
          channel: n.channel,
          participants: n.participants,
          generation: n.generation,
          mediaKey: n.media_key,
          topology: n.topology,
          sfuHost: n.sfu_host,
          sfuHostUser: n.sfu_host_user,
        });
        break;
      }
      case 'blob.progress':
        blobProgress ??= { ...s.blobProgress };
        blobProgress[n.blob] = { have: n.have, total: n.total };
        break;
      case 'net.status': {
        const { type: _t, ...rest } = n;
        net = rest;
        break;
      }
      case 'transfer.rates': {
        // 묶음 안에서는 마지막 값만 씁니다(1초마다 오므로 보통 하나).
        const { type: _t, ...rest } = n;
        rates = rest;
        break;
      }
      case 'confirm.pending': {
        const { type: _t, ...rest } = n;
        confirmations ??= s.confirmations.slice();
        if (!confirmations.some((c) => c.id === rest.id)) {
          confirmations.push(rest);
          effects.push({ type: 'toast', kind: 'warn', text: t('확인 필요: {summary}', { summary: rest.summary }) });
        }
        break;
      }
      case 'seed.coverage_warning': {
        const v = (views ?? s.views)[n.space];
        const name = v?.channels.find((c) => c.channel === n.channel)?.settings.name;
        effects.push({
          type: 'toast',
          kind: 'warn',
          text: t('#{name}에 온라인 시더가 없습니다. 다른 사람이 오프라인일 때 기록을 못 받을 수 있습니다.', { name: name ?? t('채널') }),
        });
        break;
      }
      case 'report.new': {
        reportsNew ??= { ...s.reportsNew };
        reportsNew[n.space] = (reportsNew[n.space] ?? 0) + 1;
        const name = (views ?? s.views)[n.space]?.settings.name;
        // 신고 내용(사유·메시지)은 알림에 없고 신고 화면에서 report.list로 읽습니다.
        const text = name ? t('「{name}」에 새 신고가 들어왔습니다(크루 설정 → 모더레이션)', { name }) : t('새 신고가 들어왔습니다(크루 설정 → 모더레이션)');
        effects.push({ type: 'toast', kind: 'warn', text });
        break;
      }
      case 'events.dropped':
        effects.push({ type: 'resync' });
        break;
      case 'net.bridges':
        // 사용자가 바꾼 것은 설정 화면이 이미 알리므로, 데몬이 스스로 켠 것만 알립니다.
        if (n.auto && n.enabled)
          effects.push({ type: 'toast', kind: 'warn', text: t('Tor에 바로 들어가지 못해 Snowflake 브리지를 켰습니다(네트워크 설정 → 브리지).') });
        break;
      case 'device.link_request':
        effects.push({ type: 'deviceLinkRequest', request: { name: n.name, code: n.code, contested: n.contested } });
        break;
      case 'device.link_result':
        effects.push({ type: 'deviceLinkResult', status: n.status });
        if (n.status === 'approved') effects.push({ type: 'loadAll' });
        break;
      // loadAll은 도는 중이면 다시 돌지 않으므로 화면을 가르는 신원은 따로 읽습니다.
      case 'device.synced':
        effects.push({ type: 'toast', kind: 'success', text: t('첫 동기화를 마쳤습니다.') });
        effects.push({ type: 'refreshIdentity' }, { type: 'loadAll' });
        break;
      case 'badges.updated':
        if (!effects.some((e) => e.type === 'badges')) effects.push({ type: 'badges' });
        break;
      case 'supporter.updated':
        effects.push({ type: 'supporter' });
        if (n.error) effects.push({ type: 'toast', kind: 'error', text: supporterErrorText(n.error) });
        break;
      case 'device.wiped':
        if (!s.accountDeleting) effects.push({ type: 'toast', kind: 'error', text: t('이 기기가 폐기되어 로컬 데이터를 모두 지웠습니다.') });
        effects.push({ type: 'resync' });
        break;
      case 'checkpoint.alert': {
        const name = (views ?? s.views)[n.space]?.settings.name ?? t('크루');
        const text =
          n.kind === 'fallback'
            ? t('「{name}」의 체크포인트 상태가 다른 멤버와 달라 처음부터 기록을 받습니다.', { name })
            : n.kind === 'equivocation'
              ? t('「{name}」에서 한 소유자 기기가 체크포인트를 둘 썼습니다. 소유자 기기가 탈취됐을 수 있습니다.', { name })
              : t('「{name}」에서 같은 순번의 다른 체크포인트를 봤습니다. 소유자 기기를 확인해 보세요.', { name });
        effects.push({ type: 'toast', kind: n.kind === 'fallback' ? 'warn' : 'error', text });
        refresh.add(n.space);
        break;
      }
    }
  }

  for (const space of refresh) effects.push({ type: 'refreshSpace', space });

  const patch: Partial<ApplyState> = {};
  if (channels) patch.channels = channels;
  if (views) patch.views = views;
  if (spaces) patch.spaces = spaces;
  if (friends) patch.friends = friends;
  if (requests) patch.requests = requests;
  if (confirmations) patch.confirmations = confirmations;
  if (blobProgress) patch.blobProgress = blobProgress;
  if (voiceRosters) patch.voiceRosters = voiceRosters;
  if (net !== undefined) patch.net = net;
  if (presence) patch.presence = presence;
  if (flair) patch.flair = flair;
  if (rates) patch.rates = rates;
  if (forumTick !== s.forumTick) patch.forumTick = forumTick;
  if (reportsNew) patch.reportsNew = reportsNew;
  if (typing) patch.typing = typing;
  return { patch, effects };
}

/** 만료 시각이 지난 휘발성 메시지를 지웁니다(데몬 만료 알림이 늦어도 화면에서는 바로 사라지게). */
export function pruneExpired(channels: Record<string, ChannelMsgs>, now: number): Record<string, ChannelMsgs> | null {
  let out: Record<string, ChannelMsgs> | null = null;
  for (const [key, cur] of Object.entries(channels)) {
    if (!cur.messages.some((m) => m.expires_at !== undefined && m.expires_at <= now)) continue;
    out ??= { ...channels };
    out[key] = { ...cur, messages: cur.messages.filter((m) => m.expires_at === undefined || m.expires_at > now) };
  }
  return out;
}

/**
 * 답장 대상으로 잡아 둔 메시지 중 휘발성 보장을 깨는 것을 지웁니다: 수명이 끝났거나 지워진 메시지,
 * 그리고 채널 목록에서 사라진(만료·삭제·다른 곳으로 이동해 버린) 휘발성 메시지.
 * 답장 미리보기는 메시지 사본을 따로 들고 있어서, 채널 내용을 지워도 여기 남으면 본문이 UI 메모리에 계속 남습니다.
 */
export function pruneReplyTo(
  replyTo: Record<string, MessageView | undefined>,
  channels: Record<string, ChannelMsgs>,
  now: number,
): Record<string, MessageView | undefined> | null {
  let out: Record<string, MessageView | undefined> | null = null;
  for (const [key, m] of Object.entries(replyTo)) {
    if (!m) continue;
    const expired = m.expires_at !== undefined && m.expires_at <= now;
    const cur = channels[key]?.messages.find((x) => x.id === m.id);
    // 일반 메시지는 채널 상태가 메모리에서 밀려나도(오래 안 본 채널) 답장 대상을 유지합니다.
    const gone = (m.volatile && !cur) || cur?.deleted === true;
    if (expired || gone || m.deleted) {
      out ??= { ...replyTo };
      delete out[key];
    }
  }
  return out;
}

/** 새 구간 목록을 기존 목록에 합칩니다(같은 구간은 하나만). */
export function mergeGaps(existing: GapView[], incoming: GapView[]): GapView[] {
  const key = (g: GapView) => `${g.after ?? ''}|${g.before ?? ''}`;
  const seen = new Set(existing.map(key));
  const out = existing.slice();
  for (const g of incoming) {
    if (!seen.has(key(g))) {
      seen.add(key(g));
      out.push(g);
    }
  }
  return out;
}
