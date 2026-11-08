// 사용자 프로필: 친구·연락처면 안전 번호 확인·로컬 메모·친구 끊기(임시 연락처는 친구 추가), 크루 안에서는 역할과 운영 동작(밴)도.
// 누구든 차단할 수 있고, 함께 쓰는 공간이 있으면 신뢰 스위치와 연결 방식(IP 주소는 보이지 않음)을 봅니다.
// 이 사람과 주고받는 속도 제한(이 기기만의 설정)도 여기서 엽니다. 그 사람의 기기 목록에서 기기 하나만 차단할 수도 있습니다
// (믿지 못할 기기를 끊는 용도).
// 프로필 탭은 그 사람이 보여 주는 정보(소개·역할)만, 설정 탭은 메모부터 이 기기에서 정하는 나머지를 담습니다.
// 이름 앞뒤에는 그 사람이 고른 뱃지 두 개, 이름 아래에는 모은 뱃지를 모두 보입니다(개발자 계정은 원년멤버를 줄 수 있음).

import { Ban, Gauge, Lock, MessageCircle, ShieldCheck, ShieldAlert, UserMinus } from 'lucide-react';
import { useEffect, useState } from 'react';
import type { Profile, SafetyNumberView, UserDeviceView } from '../api/types';
import { BadgeIcon, ShownBadges, useBadges } from '../components/Badges';
import { Avatar, EmojiText, ProposeFriendButton, Spinner, StillImg, avatarIndex, useImageBlobSrc } from '../components/common';
import { t, tc } from '../i18n';
import { formatExpiresIn, formatLastSeen, groupDigits } from '../lib/format';
import { byLevelDesc, displayName, hasPerm, outranks, roleColor } from '../lib/misc';
import { useStore } from '../store/store';
import { Modal } from './Modal';
import { TrustSection } from './Trust';

export function FriendProfileModal({ user, tab: initialTab, onClose }: { user: string; tab?: 'settings'; onClose: () => void }) {
  const client = useStore((s) => s.client);
  const me = useStore((s) => s.me);
  const friends = useStore((s) => s.friends);
  const selected = useStore((s) => s.selected);
  const views = useStore((s) => s.views);
  const call = useStore((s) => s.call);
  const toast = useStore((s) => s.toast);
  const openModal = useStore((s) => s.openModal);
  const openFriend = useStore((s) => s.openFriend);
  const refreshFriends = useStore((s) => s.refreshFriends);
  const blockUser = useStore((s) => s.blockUser);
  const unblockUser = useStore((s) => s.unblockUser);
  const canBlock = useStore((s) => s.blocked !== null);
  const isBlocked = useStore((s) => !!s.blocked?.some((b) => b.user === user));
  const blockedDevices = useStore((s) => s.blockedDevices);
  const blockDevice = useStore((s) => s.blockDevice);
  const unblockDevice = useStore((s) => s.unblockDevice);
  // 이 사람의 기기(이 기기가 아는 것). 기기 차단을 모르는 데몬이면 null로 두고 그리지 않습니다.
  const [devices, setDevices] = useState<UserDeviceView[] | null>(null);
  // 배너는 아바타와 같이 그 사람이 멤버인 공간을 통해 읽습니다.
  const imageSpace = useStore((s) => avatarIndex(s).get(user)?.split('|')[0] ?? friends.find((f) => f.user === user)?.space);
  const friend = friends.find((f) => f.user === user);
  const isFriend = !!friend;
  const space = selected.space !== 'home' ? views[selected.space] : undefined;
  const member = space?.members.find((m) => m.user === user);
  const [profile, setProfile] = useState<Profile | null>(friend?.profile ?? member?.profile ?? null);
  const [safety, setSafety] = useState<SafetyNumberView | null>(null);
  const [note, setNote] = useState('');
  const [noteSaved, setNoteSaved] = useState('');
  const [tab, setTab] = useState<'profile' | 'settings'>(initialTab ?? 'profile');
  const [badges] = useBadges(user);

  useEffect(() => {
    if (!client) return;
    let stale = false;
    client
      .call('profile.get', { user })
      .then((p) => !stale && setProfile(p))
      .catch(() => undefined);
    client
      .call('user.devices', { user })
      .then((d) => !stale && setDevices(d))
      .catch(() => undefined);
    client
      .call('note.get', { user })
      .then((n) => {
        if (stale) return;
        setNote(n.text);
        setNoteSaved(n.text);
      })
      .catch(() => undefined);
    if (isFriend) {
      client
        .call('friend.safety_number', { user })
        .then((s) => !stale && setSafety(s))
        .catch(() => undefined);
    }
    return () => {
      stale = true;
    };
  }, [client, user, isFriend]);

  const banner = useImageBlobSrc(imageSpace, profile?.banner ?? null);
  const name = member ? displayName(member) : profile?.name ?? t('알 수 없음');
  const roles = space && member ? space.roles.filter((r) => member.roles.includes(r.id)).sort(byLevelDesc) : [];
  // 접속·마지막 동기화(숨기 중인 사람은 오프라인, 비공개면 시각 없음).
  const shown = friend ?? member;
  const isMe = user === me;

  const verify = async (verified: boolean) => {
    if (!client) return;
    const r = await call(client.call('friend.verify', { user, verified }), { ok: verified ? t('안전 번호를 확인함으로 표시했습니다') : t('확인 표시를 지웠습니다') });
    if (r !== undefined) {
      setSafety((s) => (s ? { ...s, verified } : s));
      void refreshFriends();
    }
  };

  return (
    <Modal onClose={onClose} size="md" bare className="profile-modal">
      <div className="pf-banner" style={{ background: profile?.theme_color ? roleColor(profile.theme_color) : undefined }}>
        {banner.src && <StillImg className="pf-banner-img" src={banner.src} onError={banner.onError} />}
      </div>
      <div className="pf-head">
        <Avatar id={user} name={name} size={88} online={shown?.online} away={shown?.away} color={profile?.theme_color} className="pf-avatar" />
        <div className="pf-actions">
          {friend && !isMe && (
            <button
              type="button"
              className="btn primary small"
              onClick={() => {
                onClose();
                void openFriend(friend.user);
              }}
            >
              <MessageCircle size={14} /> {t('대화')}
            </button>
          )}
          {friend && !isMe && <ProposeFriendButton friend={friend} />}
        </div>
      </div>
      <div className="pf-body">
        <h2 className="pf-name">
          <ShownBadges view={badges} after={false} />
          {name}
          <ShownBadges view={badges} after />
          {member?.automation && <span className="tag bot">AI</span>}
          {friend?.kind === 'temporary' && (
            <span className="tag warn">
              {t('임시 연락처')} · {formatExpiresIn(friend.expires_at)}
            </span>
          )}
          {friend?.kind === 'expired' && <span className="tag">{t('만료된 연락처')}</span>}
        </h2>
        {member?.nickname && profile && <div className="muted small">{t('원래 이름: {name}', { name: profile.name })}</div>}
        {!!badges?.earned.length && (
          <div className="pf-badges">
            {badges.earned.map((b) => (
              <BadgeIcon key={b} badge={b} />
            ))}
          </div>
        )}
        {profile?.status && <div className="pf-status">{profile.status}</div>}
        {shown && !isMe && (
          <div className="muted small">
            {shown.online ? (shown.away ? t('자리 비움') : t('온라인')) : shown.last_seen !== undefined ? formatLastSeen(shown.last_seen) : t('오프라인')}
          </div>
        )}
        {/* 내 프로필에는 설정 탭에 들어갈 항목이 없어 탭을 두지 않습니다. */}
        {!isMe && (
          <div className="seg modal-tabs" role="tablist">
            <button type="button" role="tab" aria-selected={tab === 'profile'} className={tab === 'profile' ? 'active' : ''} onClick={() => setTab('profile')}>
              {t('프로필')}
            </button>
            <button type="button" role="tab" aria-selected={tab === 'settings'} className={tab === 'settings' ? 'active' : ''} onClick={() => setTab('settings')}>
              {t('설정')}
            </button>
          </div>
        )}
        {tab === 'profile' && (
          <div className="pf-section">
            <div className="pf-label">{t('소개')}</div>
            <div className="pre">{profile?.bio || <span className="muted">{t('소개가 없습니다')}</span>}</div>
          </div>
        )}
        {tab === 'profile' && roles.length > 0 && (
          <div className="pf-section">
            <div className="pf-label">{t('역할')}</div>
            <div className="chips">
              {roles.map((r) => (
                <span key={r.id} className="role-chip">
                  <span className="role-dot" style={{ background: roleColor(r.color) ?? 'var(--text-muted)' }} />
                  <EmojiText text={r.name} />
                </span>
              ))}
            </div>
          </div>
        )}
        {tab === 'settings' && (
          <div className="pf-section">
            <div className="pf-label">{t('메모(이 기기에만, 상대에게 안 보임)')}</div>
            <textarea className="input" rows={2} maxLength={4000} value={note} onChange={(e) => setNote(e.target.value)} placeholder={t('메모를 남겨 두세요')} />
            {note !== noteSaved && (
              <button
                type="button"
                className="btn small primary"
                onClick={async () => {
                  if (!client) return;
                  const r = await call(client.call('note.set', { user, text: note }));
                  if (r !== undefined) {
                    setNoteSaved(note);
                    toast('success', t('메모를 저장했습니다'));
                  }
                }}
              >
                {t('메모 저장')}
              </button>
            )}
          </div>
        )}
        {tab === 'settings' && friend && (
          <div className="pf-section">
            <div className="pf-label">
              {t('안전 번호')}{' '}
              {safety?.verified ? (
                <span className="tag ok">
                  <ShieldCheck size={11} /> {t('확인함')}
                </span>
              ) : (
                <span className="tag warn">
                  <ShieldAlert size={11} /> {t('확인 안 함')}
                </span>
              )}
            </div>
            {safety ? (
              <>
                <div className="safety">
                  {groupDigits(safety.digits).map((g, i) => (
                    <span key={i}>{g}</span>
                  ))}
                </div>
                <p className="muted small">
                  {t('만나서 또는 다른 믿을 수 있는 경로로 서로의 번호가 같은지 비교하세요. 같으면 중간에서 가로챈 사람이 없습니다.')}
                </p>
                <button type="button" className="btn small" onClick={() => void verify(!safety.verified)}>
                  <Lock size={13} /> {safety.verified ? t('확인 표시 지우기') : t('같은 번호임을 확인함')}
                </button>
              </>
            ) : (
              <Spinner />
            )}
          </div>
        )}
        {tab === 'settings' && <TrustSection user={user} name={name} />}
        {tab === 'settings' && blockedDevices !== null && devices && devices.length > 0 && (
          <div className="pf-section">
            <div className="pf-label">{t('기기')}</div>
            <div className="table">
              {devices.map((d) => {
                const blockedHere = blockedDevices.some((b) => b.device === d.device);
                const label = d.name || t('이름 없는 기기');
                return (
                  <div key={d.device} className="tr">
                    <div className="grow">
                      <div className="row gap-s">
                        <strong>{label}</strong>
                        {blockedHere && <span className="tag warn">{t('차단함')}</span>}
                      </div>
                      <div className="muted small mono">{d.device.slice(0, 16)}…</div>
                    </div>
                    {blockedHere ? (
                      <button type="button" className="btn small" onClick={() => void unblockDevice(user, d.device)}>
                        {t('차단 풀기')}
                      </button>
                    ) : (
                      <button type="button" className="btn small danger-outline" onClick={() => blockDevice(user, d.device, label)}>
                        <Ban size={13} /> {t('기기 차단')}
                      </button>
                    )}
                  </div>
                );
              })}
            </div>
          </div>
        )}
        {tab === 'settings' && (
          <div className="row gap-s wrap">
            <button
              type="button"
              className="btn small"
              title={t('이 사람과 주고받는 올리기·받기 속도 제한(이 기기만의 설정)')}
              onClick={() => openModal({ kind: 'rateLimit', scope: { kind: 'user', user }, back: { kind: 'friendProfile', user, tab: 'settings' } })}
            >
              <Gauge size={14} /> {t('속도 제한')}
            </button>
            {canBlock &&
              (isBlocked ? (
                <button type="button" className="btn small" onClick={() => void unblockUser(user)}>
                  <Ban size={14} /> {t('차단 풀기')}
                </button>
              ) : (
                <button type="button" className="btn small danger-outline" onClick={() => blockUser(user, name)}>
                  <Ban size={14} /> {tc('차단하기 단추', '차단')}
                </button>
              ))}
            {space && space.kind === 'crew' && member && hasPerm(space, 'ban') && outranks(space, me, user) && (
              <button type="button" className="btn small danger-outline" onClick={() => openModal({ kind: 'ban', space: space.space, user })}>
                <Ban size={14} /> {t('이 크루에서 밴')}
              </button>
            )}
          </div>
        )}
        {(isMe || tab === 'settings') && badges?.can_grant_founder && (
          <button
            type="button"
            className="btn small"
            onClick={() =>
              openModal({
                kind: 'confirm',
                title: t('원년멤버 뱃지 주기'),
                body: t('{name} 님에게 원년멤버 뱃지를 줍니다. 되돌릴 수 없습니다.', { name }),
                confirmLabel: t('주기'),
                onConfirm: async () => {
                  if (client) await call(client.call('badge.grant_founder', { user }), { ok: t('원년멤버 뱃지를 줬습니다') });
                },
              })
            }
          >
            <BadgeIcon badge="founder" /> {t('원년멤버 뱃지 주기')}
          </button>
        )}
        {tab === 'settings' && friend && (
          <button
            type="button"
            className="btn small danger-outline"
            onClick={() =>
              openModal({
                kind: 'confirm',
                title: friend.kind === 'friend' ? t('친구 끊기') : t('연락처 지우기'),
                body:
                  friend.kind === 'friend'
                    ? t('{name} 님과 친구를 끊습니다. 양쪽의 1대1 대화 기록과 파일이 모두 지워지며 되돌릴 수 없습니다.', { name: friend.profile.name })
                    : t('{name} 님을 연락처에서 지웁니다. 양쪽의 1대1 대화 기록과 파일이 모두 지워지며 되돌릴 수 없습니다.', { name: friend.profile.name }),
                danger: true,
                confirmLabel: friend.kind === 'friend' ? t('친구 끊기') : t('지우기'),
                onConfirm: async () => {
                  if (!client) return;
                  await call(client.call('friend.remove', { user }));
                  await refreshFriends();
                },
              })
            }
          >
            <UserMinus size={14} /> {friend.kind === 'friend' ? t('친구 끊기') : t('연락처 지우기')}
          </button>
        )}
      </div>
    </Modal>
  );
}
