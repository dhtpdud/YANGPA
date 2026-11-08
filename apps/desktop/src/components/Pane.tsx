// 가운데 영역: 상단 막대 + 채널 종류별 화면(텍스트·포럼·음성) + 휘발성 안내 + 참가 전 P2P 전용 채널의 잠금 화면.

import {
  AlertTriangle,
  Bot,
  Check,
  ChevronDown,
  ChevronUp,
  Clock,
  DoorOpen,
  Flame,
  Images,
  Lock,
  LogIn,
  Search,
  ShieldCheck,
  ShieldOff,
  Unplug,
  Users,
  X,
} from 'lucide-react';
import { useState } from 'react';
import type { ChannelView, SpaceView } from '../api/types';
import { t } from '../i18n';
import { isP2pLocked, leaveEffectText, volatileLifetimeText } from '../lib/channel';
import { formatDuration, formatExpiresIn, formatTtl } from '../lib/format';
import { cx, displayName } from '../lib/misc';
import { applyAuthor, authorSuggestions } from '../lib/search';
import { viewerAvailable } from '../supporter/viewer';
import { isNarrow } from '../lib/platform';
import { chKey, findChannel, useStore } from '../store/store';
import { CaptureShield } from './CaptureShield';
import { AnonIcon, Avatar, ChannelIcon, Empty, P2pIcon, ProposeFriendButton, Spinner, VolatileBadge } from './common';
import { Composer } from './Composer';
import { ForumView } from './ForumView';
import { MessageList } from './MessageList';
import { BackButton } from './BackButton';
import { VoiceView } from './VoiceView';
import { SearchResults } from './SearchResults';

export function TopBar({ space, channel }: { space: SpaceView; channel?: ChannelView }) {
  const prefs = useStore((s) => s.prefs);
  const setPrefs = useStore((s) => s.setPrefs);
  const leaveChannel = useStore((s) => s.leaveChannel);
  const protection = useStore((s) => s.protection);
  const runSearch = useStore((s) => s.runSearch);
  const clearSearch = useStore((s) => s.clearSearch);
  const search = useStore((s) => s.search);
  const setApiReadAllowed = useStore((s) => s.setApiReadAllowed);
  const leaveP2p = useStore((s) => s.leaveP2p);
  const openModal = useStore((s) => s.openModal);
  const setMobilePane = useStore((s) => s.setMobilePane);
  const [q, setQ] = useState('');
  const [searchFocus, setSearchFocus] = useState(false);
  const [sugSel, setSugSel] = useState(0);
  const volatile = channel?.settings.security === 'volatile';
  // 'from:'을 쓰는 중이면 이름이 맞는 멤버를, 빈 칸이면 작성자 검색 방법을 보여 줍니다(크루·1대1만).
  const authors = searchFocus ? authorSuggestions(q, space.members) : null;
  const pickAuthor = (i: number) => {
    const m = authors?.[i];
    if (m) setQ(applyAuthor(q, m));
    setSugSel(0);
  };
  const joinLeave = channel?.settings.join_mode === 'join_leave';
  const peer = space.kind === 'space' ? space.members.find((m) => m.user !== space.owner) ?? space.members[1] : undefined;
  return (
    <header className="topbar">
      <div className="tb-left">
        <BackButton />
        {channel ? (
          <>
            <ChannelIcon channel={channel} size={22} />
            <h1 className="tb-title">{channel.settings.name}</h1>
            <VolatileBadge channel={channel} />
            {channel.p2p && (
              <span
                className="badge p2p"
                title={
                  channel.p2p_joined
                    ? t('이 기기는 참가 중입니다. 이 채널의 내용은 참가한 기기끼리 직접 오가고, 참가자끼리 서로 IP가 보입니다.')
                    : t('이 기기는 아직 참가하지 않아 채널이 잠겨 있습니다.')
                }
              >
                <P2pIcon size={11} /> {t('P2P 전용')}
              </span>
            )}
            {channel.anonymous && (
              <span className="badge anon" title={t("익명 채널: 글, 답글, 반응, 통화 참가자가 '익명'으로 보입니다. 꺼도 켜져 있는 동안 쓴 글은 익명으로 남습니다.")}>
                <AnonIcon size={11} /> {t('익명')}
              </span>
            )}
            {joinLeave && <span className="badge">{t('입장·퇴장')}</span>}
            {volatile && channel.api_read_allowed && (
              <button
                type="button"
                className="badge api-read"
                title={t('이 기기의 API 토큰(AI·자동화 포함)이 이 휘발성 채널을 읽을 수 있습니다. 누르면 다시 막습니다.')}
                onClick={() => setApiReadAllowed(space.space, channel.channel, false)}
              >
                <Bot size={11} strokeWidth={2.5} /> {t('AI 읽기 허용')}
              </button>
            )}
            {channel.settings.topic && (
              <>
                <span className="tb-divider" />
                <span className="tb-topic" title={channel.settings.topic}>
                  {channel.settings.topic}
                </span>
              </>
            )}
          </>
        ) : (
          <h1 className="tb-title">{space.kind === 'space' ? displayName(peer) : space.settings.name}</h1>
        )}
      </div>
      <div className="tb-right">
        {volatile && (
          <span
            className={cx('tb-protect', protection.supported === false ? 'off' : 'on')}
            title={
              protection.supported === false
                ? t('이 운영체제(리눅스 등)에서는 창 캡처 차단을 지원하지 않습니다.')
                : t('휘발성 채널에서는 늘 창 캡처를 막습니다: 화면 녹화·스크린샷에 이 창 내용이 나오지 않습니다.')
            }
          >
            {protection.supported === false ? <ShieldOff size={16} /> : <ShieldCheck size={16} />}
            {protection.supported === false ? t('캡처 차단 안 됨') : t('캡처 차단')}
          </span>
        )}
        {channel?.p2p && channel.p2p_joined && (
          <button
            type="button"
            className="btn small danger-outline"
            title={t('이 기기의 P2P 참여를 그만둡니다. 채널이 다시 잠기고, 다시 쓰려면 경고를 보고 참가해야 합니다.')}
            onClick={() => void leaveP2p(space.space, channel.channel)}
          >
            <Unplug size={14} /> {t('P2P 참여 그만두기')}
          </button>
        )}
        {channel && joinLeave && channel.joined && (
          <button type="button" className="btn small danger-outline" onClick={() => void leaveChannel(space.space, channel.channel)}>
            <DoorOpen size={14} /> {t('퇴장')}
          </button>
        )}
        <form
          className="tb-search"
          onSubmit={(e) => {
            e.preventDefault();
            void runSearch(q);
          }}
        >
          <Search size={14} />
          <input
            placeholder={t('검색 (from:이름 = 작성자)')}
            title={t('본문을 찾습니다(일반 등급만). from:이름 을 쓰면 그 사람이 쓴 글만 찾습니다.')}
            value={q}
            onChange={(e) => {
              setQ(e.target.value);
              setSugSel(0);
            }}
            onFocus={() => setSearchFocus(true)}
            onBlur={() => setSearchFocus(false)}
            onKeyDown={(e) => {
              if (e.nativeEvent.isComposing || !authors?.length) return;
              if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
                e.preventDefault();
                setSugSel((sugSel + (e.key === 'ArrowDown' ? 1 : authors.length - 1)) % authors.length);
              } else if (e.key === 'Enter' || e.key === 'Tab') {
                e.preventDefault();
                pickAuthor(sugSel);
              }
            }}
          />
          {searchFocus && (authors ? authors.length > 0 : !q) && (
            <div className="tb-suggest">
              {authors ? (
                <>
                  <div className="suggest-title">{t('작성자')}</div>
                  {authors.map((m, i) => (
                    <button
                      key={m.user}
                      type="button"
                      className={cx('suggest-item', i === sugSel && 'active')}
                      onMouseDown={(e) => e.preventDefault()}
                      onClick={() => pickAuthor(i)}
                    >
                      <Avatar id={m.user} name={displayName(m)} size={20} />
                      <span>{displayName(m)}</span>
                    </button>
                  ))}
                </>
              ) : (
                <button type="button" className="suggest-item" onMouseDown={(e) => e.preventDefault()} onClick={() => setQ('from:')}>
                  <strong>from:</strong>
                  <span className="muted">{t('이름 — 그 사람이 쓴 글 찾기')}</span>
                </button>
              )}
            </div>
          )}
          {search && (
            <button
              type="button"
              className="icon-btn small"
              onClick={() => {
                setQ('');
                clearSearch();
              }}
            >
              <X size={13} />
            </button>
          )}
        </form>
        {viewerAvailable(channel) && (
          <button type="button" className="icon-btn" title={t('콘텐츠 뷰어')} onClick={() => channel && openModal({ kind: 'viewer', space: space.space, channel: channel.channel })}>
            <Images size={20} />
          </button>
        )}
        <button
          type="button"
          className={cx('icon-btn', prefs.memberList && 'active')}
          title={t('멤버 목록')}
          onClick={() => (isNarrow() ? setMobilePane('members') : setPrefs({ memberList: !prefs.memberList }))}
        >
          <Users size={20} />
        </button>
      </div>
    </header>
  );
}

export function VolatileBanner({ space, channel }: { space: string; channel: ChannelView }) {
  const collapsed = useStore((s) => s.prefs.bannerCollapsed);
  const setPrefs = useStore((s) => s.setPrefs);
  const setApiReadAllowed = useStore((s) => s.setApiReadAllowed);
  const apiRead = channel.api_read_allowed;
  const protection = useStore((s) => s.protection);
  const ttl = formatTtl(channel.settings.ttl);
  return (
    <div className={cx('volatile-banner', collapsed && 'collapsed')}>
      <button type="button" className="vb-head" onClick={() => setPrefs({ bannerCollapsed: !collapsed })}>
        <Flame size={16} />
        <strong>{t('휘발성 채널')}</strong>
        <span className="vb-sub">
          {ttl ?? t('수명 없음')}
          {channel.settings.join_mode === 'join_leave' ? ' · ' + t('들어와 있는 동안만') : ''} · {t('디스크에 저장하지 않음')}
        </span>
        {collapsed ? <ChevronDown size={16} /> : <ChevronUp size={16} />}
      </button>
      {!collapsed && (
        <div className="vb-body">
          <div className="vb-col ok">
            <div className="vb-title">{t('보장하는 것')}</div>
            <ul>
              <li>
                <Check size={13} /> {t('메시지·첨부를 디스크·로그·캐시에 쓰지 않고 메모리에만 둡니다')}
              </li>
              <li>
                <Check size={13} /> {volatileLifetimeText(channel.settings)}
              </li>
              <li>
                <Check size={13} /> {t('알림에는 "새 메시지"만 나오고 보낸 사람·내용은 나오지 않습니다')}
              </li>
              <li>
                <Check size={13} /> {protection.supported === false ? t('창 캡처 차단(이 운영체제는 지원 안 함)') : t('창 캡처 차단')}
              </li>
            </ul>
          </div>
          <div className="vb-col no">
            <div className="vb-title">{t('막을 수 없는 것')}</div>
            <ul>
              <li>
                <AlertTriangle size={13} /> {t('스크린샷·화면 녹화·다른 기기로 화면 촬영')}
              </li>
              <li>
                <AlertTriangle size={13} /> {t('수정된 클라이언트가 받은 내용을 몰래 저장하는 것')}
              </li>
              <li>
                <AlertTriangle size={13} /> {t('복사해 둔 내용(클립보드에 남음)')}
              </li>
            </ul>
          </div>
          <div className={cx('vb-api', apiRead && 'on')}>
            <Bot size={14} />
            <div className="grow">
              <strong>{apiRead ? t('AI·자동화 읽기(이 기기): 허용됨') : t('AI·자동화 읽기(이 기기): 막힘')}</strong>
              <div className="small">
                {apiRead
                  ? t('이 기기의 API 토큰이 이 채널을 읽을 수 있습니다. 읽어 간 내용은 그쪽에 남을 수 있습니다.')
                  : t('AI가 읽는 순간 사실상 기록이 되므로 기본으로 막혀 있습니다. 이 기기의 API 토큰에만 적용됩니다.')}
              </div>
            </div>
            <button
              type="button"
              className={cx('btn small', apiRead ? 'primary' : 'danger-outline')}
              onClick={() => setApiReadAllowed(space, channel.channel, !apiRead)}
            >
              {apiRead ? t('다시 막기') : t('허용하기…')}
            </button>
          </div>
        </div>
      )}
    </div>
  );
}

/** 상시 휘발성 채널의 참가 상한이 차서 이 기기가 참가하지 못하는 동안(자리가 나면 데몬이 알아서 참가시킴). */
function FullGate({ channel }: { channel: ChannelView }) {
  return (
    <div className="join-gate">
      <div className="jg-icon">
        <Flame size={40} />
      </div>
      <h2>#{channel.settings.name}</h2>
      <ul className="jg-list">
        <li>{t('상시 참여 인원이 가득 찼습니다(최대 {n}대).', { n: channel.volatile_cap ?? 0 })}</li>
        <li>{t('자리가 나면 자동으로 참여합니다. 그 전에 오간 메시지는 이 기기에 오지 않습니다.')}</li>
      </ul>
    </div>
  );
}

function JoinGate({ space, channel }: { space: string; channel: ChannelView }) {
  const joinChannel = useStore((s) => s.joinChannel);
  const [busy, setBusy] = useState(false);
  const s = channel.settings;
  // 수명 문장은 문장째 번역하려고 종류별로 따로 씁니다.
  const ttl =
    s.ttl.mode === 'on_leave'
      ? t('메시지는 퇴장 시 삭제됩니다.')
      : s.ttl.mode === 'seconds'
        ? t('메시지는 {time} 후 삭제됩니다.', { time: formatDuration(s.ttl.seconds) })
        : null;
  return (
    <div className="join-gate">
      <div className="jg-icon">{s.security === 'volatile' ? <Flame size={40} /> : <LogIn size={40} />}</div>
      <h2>#{s.name}</h2>
      <p className="muted">{s.topic}</p>
      <ul className="jg-list">
        <li>{t('들어와 있는 동안만 대화를 받습니다. {effect}', { effect: leaveEffectText(s) })}</li>
        <li>
          {s.history === 'private'
            ? t('입장한 뒤의 메시지만 볼 수 있습니다.')
            : s.security === 'volatile'
              ? t('입장하면 지금 참가자들의 메모리에 남아 있는 메시지를 받습니다.')
              : t('입장하면 이전 기록도 볼 수 있습니다.')}
        </li>
        {ttl && <li>{ttl}</li>}
        <li>{t('지금 {n}명이 들어와 있습니다.', { n: channel.participants?.length ?? 0 })}</li>
      </ul>
      <button
        type="button"
        className="btn primary"
        disabled={busy}
        onClick={async () => {
          setBusy(true);
          await joinChannel(space, channel.channel);
          setBusy(false);
        }}
      >
        {busy ? <Spinner size={14} /> : <LogIn size={16} />} {t('입장하기')}
      </button>
    </div>
  );
}

/**
 * 이 기기가 아직 참가하지 않은 P2P 전용 채널. 내용·입력창·통화 대신 무엇이 드러나는지 알리고, 사용자가 직접 누른
 * 경우에만 참가합니다(`channel.p2p_join`). 참가는 이 기기에만 적용됩니다.
 */
export function P2pGate({ space, channel }: { space: string; channel: ChannelView }) {
  const joinP2p = useStore((s) => s.joinP2p);
  const [busy, setBusy] = useState(false);
  const s = channel.settings;
  const voice = s.kind === 'voice';
  return (
    <div className="join-gate p2p-gate">
      <div className="jg-icon p2p">
        <Lock size={36} />
      </div>
      <h2>{voice ? s.name : `#${s.name}`}</h2>
      <p className="muted row gap-s">
        <P2pIcon /> {t('P2P 전용 채널 · 이 기기는 아직 참가하지 않았습니다')}
      </p>
      <div className="warn-box p2p-warn" role="alert">
        <AlertTriangle size={16} />
        <div>
          <strong>{t('이 채널의 참가자는 내 IP 주소를 볼 수 있습니다.')}</strong>
          <div>
            {voice
              ? t('통화가 Tor를 거치지 않고 참가한 기기끼리 직접 오갑니다. IP 주소로는 대략의 위치와 인터넷 회사를 알 수 있습니다.')
              : t('메시지·파일·통화가 Tor를 거치지 않고 참가한 기기끼리 직접 오갑니다. IP 주소로는 대략의 위치와 인터넷 회사를 알 수 있습니다.')}
          </div>
        </div>
      </div>
      <ul className="jg-list">
        <li>{t('믿을 수 있는 사람들끼리만 쓰세요. 참가하지 않으면 이 채널은 이 기기에서 잠긴 채로 남습니다.')}</li>
        <li>{t('참가는 이 기기에만 적용됩니다. 다른 기기에서 쓰려면 그 기기에서도 따로 참가해야 합니다.')}</li>
        <li>{t('언제든 참여를 그만둘 수 있지만, 이미 다른 참가자에게 보인 IP 주소는 다시 숨길 수 없습니다.')}</li>
      </ul>
      <button
        type="button"
        className="btn primary"
        disabled={busy}
        onClick={async () => {
          setBusy(true);
          await joinP2p(space, channel.channel);
          setBusy(false);
        }}
      >
        {busy ? <Spinner size={14} /> : <P2pIcon size={16} />} {t('참가')}
      </button>
    </div>
  );
}

function ConversionBanner({ space }: { space: SpaceView }) {
  const me = useStore((s) => s.me);
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  if (!space.pending_crew_conversion) return null;
  const mine = space.pending_crew_conversion === me;
  return (
    <div className="info-banner">
      <Users size={16} />
      {mine ? (
        <span>{t('크루 전환을 요청했습니다. 상대가 동의하면 크루로 바뀝니다(기록은 그대로).')}</span>
      ) : (
        <>
          <span>{t('상대가 이 대화를 크루로 바꾸자고 요청했습니다. 둘 다 동의해야 바뀝니다.')}</span>
          <button
            type="button"
            className="btn small primary"
            onClick={() => client && void call(client.call('crew.convert_accept', { space: space.space }), { ok: t('크루로 전환했습니다') })}
          >
            {t('동의')}
          </button>
        </>
      )}
    </div>
  );
}

/** 1대1 대화 상대가 임시 연락처면 남은 시간과 친구 추가를, 만료된 연락처면 읽기만 된다는 안내를 띠로 보입니다. */
export function ContactStrip({ space }: { space: SpaceView }) {
  const friend = useStore((s) => (space.kind === 'space' ? s.friends.find((f) => f.space === space.space) : undefined));
  if (!friend || friend.kind === 'friend') return null;
  if (friend.kind === 'expired') {
    return (
      <div className="contact-strip expired" role="note">
        <Clock size={14} />
        <span>{t('만료된 연락처입니다. 새 연락처 링크를 받으면 다시 대화할 수 있습니다.')}</span>
      </div>
    );
  }
  return (
    <div className="contact-strip">
      <Clock size={14} />
      <span className="grow">{t('임시 연락처')} · {formatExpiresIn(friend.expires_at)}</span>
      <ProposeFriendButton friend={friend} />
    </div>
  );
}

export function SpacePane({ space }: { space: SpaceView }) {
  const selected = useStore((s) => s.selected);
  const views = useStore((s) => s.views);
  const channel = selected.space === space.space ? findChannel(views, space.space, selected.channel) : undefined;
  const volatile = channel?.settings.security === 'volatile';
  const locked = isP2pLocked(channel);
  // 입장·퇴장 채널은 들어와 있을 때만, P2P 전용 채널은 참가했을 때만 내용이 보이므로 그때만 캡처를 지켜봅니다.
  const watchCapture = !!channel && volatile && !locked && (channel.settings.join_mode !== 'join_leave' || channel.joined);
  const search = useStore((s) => s.search);

  let body;
  if (!channel) {
    body = <Empty title={t('채널을 고르세요')} />;
  } else if (locked) {
    // 텍스트든 음성이든 참가하기 전에는 내용·입력창·통화를 열지 않습니다.
    body = (
      <>
        {volatile && <VolatileBanner space={space.space} channel={channel} />}
        <P2pGate key={chKey(space.space, channel.channel)} space={space.space} channel={channel} />
      </>
    );
  } else if (channel.settings.kind === 'voice') {
    body = <VoiceView key={chKey(space.space, channel.channel)} space={space} channel={channel} />;
  } else if (channel.settings.kind === 'forum') {
    // 채널마다 새로 그립니다(열어 둔 글·목록이 다른 포럼 채널로 새지 않게).
    body = <ForumView key={chKey(space.space, channel.channel)} space={space} channel={channel} />;
  } else if (volatile && channel.volatile_full) {
    body = (
      <>
        <VolatileBanner space={space.space} channel={channel} />
        <FullGate channel={channel} />
      </>
    );
  } else if (channel.settings.join_mode === 'join_leave' && !channel.joined) {
    body = (
      <>
        {volatile && <VolatileBanner space={space.space} channel={channel} />}
        <JoinGate space={space.space} channel={channel} />
      </>
    );
  } else {
    const placeholder = t('{channel}에 메시지 보내기', { channel: `${volatile ? '🔥 ' : '#'}${channel.settings.name}` });
    body = (
      <>
        {volatile && <VolatileBanner space={space.space} channel={channel} />}
        <MessageList key={channel.channel} space={space.space} channel={channel.channel} channelView={channel} />
        <Composer key={chKey(space.space, channel.channel)} space={space.space} channel={channel.channel} channelView={channel} placeholder={placeholder} />
      </>
    );
  }
  return (
    <main className={cx('pane', volatile && 'volatile')}>
      <TopBar space={space} channel={channel} />
      <ConversionBanner space={space} />
      <ContactStrip space={space} />
      <div className="pane-body">
        <div className="pane-main">{body}</div>
        {search && <SearchResults space={space} />}
      </div>
      {watchCapture && <CaptureShield key={chKey(space.space, channel.channel)} />}
    </main>
  );
}
