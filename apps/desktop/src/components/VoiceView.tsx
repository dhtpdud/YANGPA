// 통화 채널 화면: 참가 전 안내, 참가 후 참가자 타일·화면공유·조작 막대.
// 상대가 화면을 공유하면 바로 보이고('그만 보기'로 끔), 보는 화면 오른쪽 아래에 소리 크기·PiP·전체화면을 둡니다.
// 위쪽 상태 줄에 지금 토폴로지(메시/SFU와 중계 기기)를, SFU에서는 보는 화면마다 받을 화질을 고르게 합니다.
// 익명 채널의 참가자는 데몬이 준 번호로 '익명 N'(내 줄은 '(나)')으로만 보입니다.

import {
  Eye,
  EyeOff,
  Headphones,
  HeadphoneOff,
  Lock,
  Maximize,
  Mic,
  MicOff,
  MonitorUp,
  MonitorX,
  Network,
  PhoneOff,
  PictureInPicture2,
  Volume2,
  VolumeX,
} from 'lucide-react';
import { useEffect, useMemo, useRef, useState } from 'react';
import type { ChannelView, MemberView, SpaceView, VoiceParticipant } from '../api/types';
import { t } from '../i18n';
import { ANON_NAME, anonParticipantLabel, isAnonymousUser } from '../lib/anon';
import { isP2pLocked } from '../lib/channel';
import { cx, displayName, hasPerm } from '../lib/misc';
import { isAndroid } from '../lib/platform';
import { chKey, useStore } from '../store/store';
import { TOR_CALL_NOTE, isPeerConnected, isTorCall, needsTorScreen } from '../voice/policy';
import { openPip, pipSupported } from '../voice/pip';
import type { ContentHint, RxState, ScreenLayer, VoiceSnapshot } from '../voice/session';
import { Avatar, Spinner } from './common';

function StreamVideo({ stream, muted = true, className }: { stream: MediaStream | undefined; muted?: boolean; className?: string }) {
  const ref = useRef<HTMLVideoElement>(null);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.srcObject = stream ?? null;
    if (stream) void el.play().catch(() => undefined);
    return () => {
      el.srcObject = null;
    };
  }, [stream]);
  return <video ref={ref} className={className} autoPlay playsInline muted={muted} disablePictureInPicture />;
}

/** 보는 화면 타일을 전체화면으로(데스크톱 앱은 창도 함께, api/client.ts). 다시 누르거나 Esc로 나옵니다. */
function toggleFullscreen(el: Element | null) {
  if (document.fullscreenElement) void document.exitFullscreen().catch(() => undefined);
  else void el?.requestFullscreen().catch(() => undefined);
}

const SFU_STATE_LABEL: Record<string, string> = {
  new: t('연결 준비'),
  connecting: t('연결 중'),
  waiting: t('다시 붙는 중'),
  disconnected: t('끊김'),
  failed: t('실패'),
  closed: t('닫힘'),
};

/** 상대 소리 진단(받고 있으면 아무것도 보이지 않음). */
const RX_LABEL: Partial<Record<RxState, string>> = {
  bad: t('소리를 풀지 못함'),
  none: t('소리가 오지 않음'),
};
const RX_HINT: Partial<Record<RxState, string>> = {
  bad: t('상대의 소리는 도착하지만 통화 암호 키가 서로 달라 풀지 못합니다. 둘 다 통화에서 나갔다가 다시 들어와 보세요.'),
  none: t('상대에게서 소리가 전혀 오지 않습니다. 상대의 마이크 장치(설정 → 음성·화면공유 → 장치)와 음소거, 연결 상태를 확인해 주세요.'),
};

const LAYER_LABEL: Record<ScreenLayer, string> = {
  auto: t('화질 자동'),
  high: t('높음'),
  medium: t('중간'),
  low: t('낮음'),
};

/** 상태 줄의 토폴로지 표시: 메시, SFU(중계 기기와 연결 상태), 넘겨받는 중. */
export function topologyLabel(
  voice: Pick<VoiceSnapshot, 'topology' | 'sfuHost' | 'sfuHostUser' | 'sfuState' | 'participants'> & Partial<Pick<VoiceSnapshot, 'connection'>>,
  myDevice: string | undefined,
  members: Map<string, MemberView>,
): { text: string; title: string } {
  if (voice.topology === 'tor') {
    // 서로 신뢰해 WebRTC로 이어진 상대(그 밖은 'tor').
    const direct = Object.values(voice.connection ?? {}).filter((s) => s === 'connected').length;
    return { text: direct > 0 ? t('Tor 통화 · {n}명과 직접 연결', { n: direct }) : t('Tor 통화 · 소리만'), title: TOR_CALL_NOTE };
  }
  if (voice.topology === 'mesh') {
    return { text: t('메시 · 서로 직접 연결'), title: t('참가자가 적어 기기끼리 직접 연결합니다(참가자 수만큼 올려 보냄).') };
  }
  const title = t('참가자가 많아 한 기기(SFU)가 받은 프레임을 풀지 않고 나눠 보냅니다. 암호화는 그대로라 중계 기기도 내용을 못 봅니다.');
  if (!voice.sfuHost) return { text: t('SFU · 중계 기기를 넘겨받는 중…'), title };
  const host = voice.sfuHost;
  const part = voice.participants.find((p) => p.device === host);
  // 통화 밖 호스트 노드(시더)도 누구 것인지 보여 줍니다: 그 노드는 참가자 IP·말하는 때·트랙 구성을 봅니다.
  const outside = voice.sfuHostUser
    ? t('{name}의 통화 밖 노드', { name: displayName(members.get(voice.sfuHostUser)) })
    : t('통화 밖 호스트 노드');
  const who = host === myDevice ? t('이 기기') : part ? participantName(part, members) : outside;
  const state = voice.sfuState && voice.sfuState !== 'connected' ? (SFU_STATE_LABEL[voice.sfuState] ?? voice.sfuState) : null;
  return { text: state ? t('SFU · 중계: {who} ({state})', { who, state }) : t('SFU · 중계: {who}', { who }), title };
}

/** 참가자 이름: 익명 채널이면 '익명 N'(내 줄은 '(나)'), 아니면 멤버 이름. */
export function participantName(p: VoiceParticipant, members: Map<string, MemberView>, isMe = false): string {
  return anonParticipantLabel(p, isMe) ?? displayName(members.get(p.user), isMe ? t('나') : t('알 수 없음'));
}

const TOR_CODEC_LABEL: Record<string, string> = { opus: 'Opus', mimi: 'Mimi', dac: 'DAC' };

const E2EE_LABEL: Record<string, string> = {
  'script-transform': 'RTCRtpScriptTransform',
  'encoded-streams': 'Encoded Streams',
  simulated: t('시뮬레이션(MOCK)'),
  none: t('꺼짐'),
};

export function VoiceView({ space, channel }: { space: SpaceView; channel: ChannelView }) {
  const voice = useStore((s) => s.voice);
  const voiceTick = useStore((s) => s.voiceTick);
  const joining = useStore((s) => s.voiceJoining);
  const identity = useStore((s) => s.identity);
  const net = useStore((s) => s.net);
  const joinVoice = useStore((s) => s.joinVoice);
  const leaveVoice = useStore((s) => s.leaveVoice);
  const setMuted = useStore((s) => s.setMuted);
  const setDeafened = useStore((s) => s.setDeafened);
  const startShare = useStore((s) => s.startShare);
  const stopShare = useStore((s) => s.stopShare);
  const setContentHint = useStore((s) => s.setContentHint);
  const watchScreen = useStore((s) => s.watchScreen);
  const setScreenLayer = useStore((s) => s.setScreenLayer);
  const setScreenVolume = useStore((s) => s.setScreenVolume);
  const toast = useStore((s) => s.toast);
  const voiceSession = useStore((s) => s.voiceSession);
  const [hint, setHint] = useState<ContentHint>('detail');
  const inCall = voice?.space === space.space && voice.channel === channel.channel;
  const members = new Map(space.members.map((m) => [m.user, m]));
  const key = chKey(space.space, channel.channel);
  const roster = useStore((s) => s.voiceRosters[key]);
  const direct = !!channel.p2p && !!channel.p2p_joined;
  const torCall = useMemo(() => isTorCall(net, direct), [net, direct]);
  void voiceTick; // 원격 스트림 갱신 신호

  if (!inCall) {
    const users = channel.participants ?? [];
    // 익명 채널은 참가자 목록(번호)이 있으면 그것으로, 없으면 수만큼 '익명'으로 그립니다.
    const anonRoster = users.some(isAnonymousUser) && roster ? roster.participants : null;
    const people = anonRoster
      ? anonRoster.map((p) => ({ key: p.device, user: p.user, name: participantName(p, members) }))
      : users.map((u, i) => ({ key: isAnonymousUser(u) ? `anon-${i}` : u, user: u, name: isAnonymousUser(u) ? ANON_NAME : displayName(members.get(u)) }));
    return (
      <div className="voice-lobby">
        <div className="vl-icon">
          <Volume2 size={40} />
        </div>
        <h2>{channel.settings.name}</h2>
        <p className="muted">{people.length ? t('{n}명이 대화 중', { n: people.length }) : t('아직 아무도 없습니다')}</p>
        <div className="vl-people">
          {people.map((p) => (
            <div key={p.key} className="vl-person">
              <Avatar id={p.user} name={p.name} size={56} />
              <span>{p.name}</span>
            </div>
          ))}
        </div>
        <button
          type="button"
          className="btn primary large"
          // 잠긴 P2P 전용 채널은 화면이 잠금 화면을 대신 보이지만, 여기서도 참가를 막습니다.
          disabled={!!joining || !hasPerm(space, 'connect_voice') || isP2pLocked(channel)}
          onClick={() => void joinVoice(space.space, channel.channel)}
        >
          {joining === key ? <Spinner size={16} /> : <Volume2 size={18} />} {t('통화 참가')}
        </button>
        <ul className="vl-notes">
          <li>
            <Lock size={13} /> {t('소리·영상 프레임을 기기에서 AES-GCM으로 암호화합니다(중계 서버도 못 봄).')}
          </li>
          {torCall ? (
            <li>{TOR_CALL_NOTE}</li>
          ) : (
            <li>{roster?.topology === 'sfu' ? t('참가자가 많아 한 기기가 중계(SFU)합니다.') : t('참가자가 적으면 서로 직접(메시) 연결합니다.')}</li>
          )}
        </ul>
      </div>
    );
  }

  const session = voiceSession();
  const myDevice = identity?.device;
  const tiles = voice.participants;
  const sharing = voice.sharing;
  const watched = tiles.filter((p) => voice.watching.includes(p.device));
  const topo = topologyLabel(voice, myDevice, members);
  // PiP 창은 앱 창의 캡처 차단을 받지 않아 휘발성 채널에서는 띄우지 않습니다.
  const canPip = pipSupported() && channel.settings.security !== 'volatile';

  const tile = (p: VoiceParticipant) => {
    const me = p.device === myDevice;
    const name = participantName(p, members, me);
    const speaking = !!voice.speaking[p.device] && !(me ? voice.muted : p.muted);
    const muted = me ? voice.muted : p.muted;
    const deafened = me ? voice.deafened : p.deafened;
    const watchingThis = voice.watching.includes(p.device);
    const conn = voice.connection[p.device];
    const up = me || isPeerConnected(voice, p);
    // 음소거한 Tor 상대는 보내는 소리가 없어 이어졌는지 알 수 없습니다(흐리게 하지 않고 따로 알림).
    const unverified = !me && up && p.muted && conn === 'tor' && !voice.heard?.includes(p.device);
    const rx = me ? undefined : voice.rx?.[p.device];
    const screenVol = voice.screenVolume[p.device] ?? 100;
    return (
      <div key={p.device} className={cx('vtile', speaking && 'speaking', watchingThis && 'has-video', !up && 'connecting')}>
        {watchingThis ? (
          <div className="vtile-video" onDoubleClick={(e) => toggleFullscreen(e.currentTarget.closest('.vtile'))}>
            <StreamVideo stream={session?.remoteScreen(p.device)} className="vtile-video" />
          </div>
        ) : me && sharing ? (
          <StreamVideo stream={session?.localScreen()} className="vtile-video local" />
        ) : (
          <Avatar id={p.user} name={name} size={72} speaking={speaking} />
        )}
        <div className="vtile-label">
          {muted && <MicOff size={14} className="red" />}
          {deafened && <HeadphoneOff size={14} className="red" />}
          <span>{name}</span>
          {/* 소리가 오지 않는다는 진단이 뜨면 그것만 보입니다(오래 기다린 뒤라 '연결 중'보다 정확). */}
          {unverified && <span className="muted small">{t('(연결 확인 전)')}</span>}
          {!up && rx !== 'none' && (
            <span className="vtile-wait">
              {!conn || conn === 'new' || conn === 'connecting' || conn === 'tor' ? (
                <>
                  <Spinner size={12} /> {t('연결 중')}
                </>
              ) : (
                `(${conn})`
              )}
            </span>
          )}
          {!me && rx && RX_LABEL[rx] && (
            <span className={cx('small', rx === 'bad' ? 'err' : 'warn')} title={RX_HINT[rx]}>
              ({RX_LABEL[rx]})
            </span>
          )}
        </div>
        {!me && p.screen && (
          <div className="vtile-actions">
            {/* 화면 소리는 메시·직접 연결로만 옵니다(SFU·Tor 상대는 화면만). */}
            {watchingThis && (voice.topology === 'mesh' || p.direct) && (
              <label className="vtile-volume" title={t('화면 소리 크기(이 통화 동안만, 스피커 음량에 곱해짐)')}>
                {screenVol === 0 ? <VolumeX size={14} /> : <Volume2 size={14} />}
                <input
                  type="range"
                  aria-label={t('화면 소리 크기')}
                  min={0}
                  max={100}
                  step={1}
                  value={screenVol}
                  onChange={(e) => setScreenVolume(p.device, Number(e.target.value))}
                />
              </label>
            )}
            <button type="button" className={cx('btn small watch', watchingThis && 'on')} onClick={() => void watchScreen(p.device, !watchingThis)}>
              {watchingThis ? <EyeOff size={14} /> : <Eye size={14} />} {watchingThis ? t('그만 보기') : t('화면 보기')}
            </button>
            {watchingThis && canPip && (
              <button
                type="button"
                className="btn small"
                title={t('작은 창(PiP)으로 보기. 화면을 보다가 다른 채널로 가거나 창을 최소화해도 저절로 바뀝니다.')}
                aria-label={t('PiP로 보기')}
                onClick={async () => {
                  const stream = session?.remoteScreen(p.device);
                  if (!stream || !(await openPip(p.device, stream))) toast('warn', t('작은 창(PiP)을 열지 못했습니다. 화면이 보이기 시작한 뒤 다시 눌러 보세요.'));
                }}
              >
                <PictureInPicture2 size={14} />
              </button>
            )}
            {watchingThis && (
              <button
                type="button"
                className="btn small"
                title={t('전체화면(두 번 눌러도 됨, Esc로 나옴)')}
                aria-label={t('전체화면')}
                onClick={(e) => toggleFullscreen(e.currentTarget.closest('.vtile'))}
              >
                <Maximize size={14} />
              </button>
            )}
          </div>
        )}
        {!me && p.screen && !watchingThis && <span className="live-badge">LIVE</span>}
        {!me && watchingThis && voice.topology === 'sfu' && (
          <select
            className="select small layer"
            aria-label={t('받을 화질')}
            title={t('받을 화질(느린 회선이면 낮게)')}
            value={voice.layers[p.device] ?? 'auto'}
            onChange={(e) => setScreenLayer(p.device, e.target.value as ScreenLayer)}
          >
            {(Object.keys(LAYER_LABEL) as ScreenLayer[]).map((l) => (
              <option key={l} value={l}>
                {LAYER_LABEL[l]}
              </option>
            ))}
          </select>
        )}
        {me && sharing && (
          <span className="live-badge">{t('LIVE · 시청 {n}명', { n: voice.viewers.length })}</span>
        )}
      </div>
    );
  };

  return (
    <div className="voice-room">
      <div className="vr-status">
        <span className="vr-e2ee" title={t('프레임 단위 종단간 암호화')}>
          <Lock size={13} />{' '}
          {voice.topology === 'tor' ? t('패킷 암호화') : t('프레임 암호화 · {mode}', { mode: E2EE_LABEL[voice.e2ee] ?? voice.e2ee })} ·{' '}
          {t('키 세대 {gen}', { gen: voice.generation })}
        </span>
        <span className={cx('vr-topology', voice.topology)} title={topo.title}>
          <Network size={13} /> {topo.text}
        </span>
        {voice.topology === 'tor' && voice.torCodec && (
          <span className="vr-neural small" title={t('Tor 통화에서 보내는 소리 코덱(고른 코덱을 모두가 풀 수 있으면 그것, 아니면 Opus)')}>
            <Mic size={13} /> {TOR_CODEC_LABEL[voice.torCodec]}
          </span>
        )}
        {voice.neural && (
          <span className="vr-neural small" title={t('신경망 음성 코덱으로 보내는 중(그 코덱 모델이 있는 상대에게만, 나머지는 Opus)')}>
            <Mic size={13} /> {voice.neural.codec === 'mimi' ? 'Mimi' : 'DAC'} · {t('{n}명', { n: voice.neural.to.length })}
          </span>
        )}
        {voice.micError && <span className="warn small">{t('마이크: {error}', { error: voice.micError })}</span>}
      </div>
      <div className={cx('vr-grid', watched.length > 0 && 'focus')}>
        {watched.length > 0 ? (
          <>
            <div className="vr-stage">{watched.map(tile)}</div>
            <div className="vr-strip">{tiles.filter((p) => !voice.watching.includes(p.device)).map(tile)}</div>
          </>
        ) : (
          tiles.map(tile)
        )}
      </div>
      <div className="vr-controls">
        <button type="button" className={cx('round-btn', voice.muted && 'off')} title={voice.muted ? t('마이크 켜기') : t('마이크 끄기')} onClick={() => void setMuted(!voice.muted)}>
          {voice.muted ? <MicOff size={22} /> : <Mic size={22} />}
        </button>
        <button type="button" className={cx('round-btn', voice.deafened && 'off')} title={voice.deafened ? t('소리 켜기') : t('소리 끄기')} onClick={() => void setDeafened(!voice.deafened)}>
          {voice.deafened ? <HeadphoneOff size={22} /> : <Headphones size={22} />}
        </button>
        {!isAndroid() && (
        <div className="share-group">
          <select
            className="select small"
            value={sharing ? voice.contentHint : hint}
            title={t('화면 내용 종류: 글자 위주면 선명하게, 게임·영상이면 부드럽게')}
            onChange={(e) => {
              const h = e.target.value as ContentHint;
              setHint(h);
              if (sharing) setContentHint(h);
            }}
          >
            <option value="detail">{t('선명하게(글자·코드)')}</option>
            <option value="motion">{t('부드럽게(게임·영상)')}</option>
          </select>
          <button
            type="button"
            className={cx('round-btn', sharing && 'live')}
            title={sharing ? t('화면공유 끄기') : needsTorScreen(voice) ? t('Go Live: 화면공유(Tor 비상용, 낮은 화질)') : t('Go Live: 화면공유')}
            disabled={!hasPerm(space, 'stream')}
            onClick={() => void (sharing ? stopShare() : startShare(hint))}
          >
            {sharing ? <MonitorX size={22} /> : <MonitorUp size={22} />}
          </button>
        </div>
        )}
        <button type="button" className="round-btn danger" title={t('나가기')} onClick={() => void leaveVoice()}>
          <PhoneOff size={22} />
        </button>
      </div>
    </div>
  );
}
