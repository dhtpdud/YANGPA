// 홈: 친구·연락처 목록(온라인·모두·대기 중)과 연락처 교환. 오른쪽에는 친구·임시 연락처·만료된 연락처를 갈래별로 보입니다.

import { Check, MessageCircle, MoreVertical, UserPlus, Users, X } from 'lucide-react';
import type { ContactKind, FriendView } from '../api/types';
import { t } from '../i18n';
import { BackButton } from './BackButton';
import { formatExpiresIn, formatLastSeen, formatRelative } from '../lib/format';
import { cx } from '../lib/misc';
import { useStore } from '../store/store';
import { AddFriendPanel } from '../modals/AddFriend';
import { Avatar, BannerBack, Empty, ProposeFriendButton } from './common';
import { openPersonMenu } from './menus';

const KINDS: ContactKind[] = ['friend', 'temporary', 'expired'];

const CONTACT_KIND_LABEL: Record<ContactKind, string> = {
  friend: t('친구'),
  temporary: t('임시 연락처'),
  expired: t('만료된 연락처'),
};

const EMPTY_KIND: Record<ContactKind, string> = {
  friend: t('아직 친구가 없습니다.'),
  temporary: t('임시 연락처가 없습니다.'),
  expired: t('만료된 연락처가 없습니다.'),
};

/** 접속 상태 문구. 앱을 켠 뒤 그 사람에게 처음 거는 연결이 끝나기 전에는 '연결 중'(점이 움직임). */
function Presence({ f }: { f: FriendView }) {
  const connecting = useStore((s) => s.net?.connecting?.includes(f.user) ?? false);
  if (f.online) return <>{f.profile.status || (f.away ? t('자리 비움') : t('온라인'))}</>;
  if (!connecting) return <>{t('오프라인')}</>;
  return (
    <span className="connecting">
      {t('연결 중')}
      <span className="typing-dots" aria-hidden>
        <i />
        <i />
        <i />
      </span>
    </span>
  );
}

/** 접속한 사람을 먼저(같으면 받은 순서 그대로). */
const onlineFirst = (list: FriendView[]) => list.slice().sort((a, b) => Number(b.online) - Number(a.online));

function FriendRow({ f }: { f: FriendView }) {
  const openFriend = useStore((s) => s.openFriend);
  const openModal = useStore((s) => s.openModal);
  const expired = f.kind === 'expired';
  return (
    <div className={cx('friend-row', expired && 'expired')} onContextMenu={(e) => openPersonMenu(e, f.user, f.profile.name)}>
      <Avatar id={f.user} name={f.profile.name} size={36} online={f.online} away={f.away} color={f.profile.theme_color} />
      <div className="fr-names">
        <span className="fr-name">
          {f.profile.name}
          {f.safety_number_verified && <span className="tag ok">{t('확인됨')}</span>}
          {f.kind === 'temporary' && <span className="tag warn">{t('임시')}</span>}
          {expired && <span className="tag">{t('만료')}</span>}
        </span>
        <span className="fr-status">
          {expired ? t('만료된 연락처 · 대화는 읽기만 할 수 있습니다') : <Presence f={f} />}
          {f.kind === 'temporary' && ` · ${formatExpiresIn(f.expires_at)}`}
        </span>
      </div>
      <ProposeFriendButton friend={f} />
      <button type="button" className="round-icon" title={expired ? t('대화 보기(읽기만)') : t('대화')} onClick={() => void openFriend(f.user)}>
        <MessageCircle size={18} />
      </button>
      <button type="button" className="round-icon" title={t('프로필·안전 번호')} onClick={() => openModal({ kind: 'friendProfile', user: f.user })}>
        <MoreVertical size={18} />
      </button>
    </div>
  );
}

/** 오른쪽 현황: 갈래마다 사람 수와 접속 상태, 임시 연락처는 남은 시간까지. */
export function ContactSidePanel() {
  const friends = useStore((s) => s.friends);
  const openModal = useStore((s) => s.openModal);
  return (
    <aside className="home-side" aria-label={t('연락처 현황')}>
      {KINDS.map((kind) => {
        const people = onlineFirst(friends.filter((f) => f.kind === kind));
        return (
          <section key={kind} className={cx('hs-group', kind)} aria-label={CONTACT_KIND_LABEL[kind]}>
            <h3>
              {CONTACT_KIND_LABEL[kind]} <span className="hs-count">{people.length}</span>
            </h3>
            {people.length === 0 ? (
              <p className="muted small">{EMPTY_KIND[kind]}</p>
            ) : (
              people.map((f) => (
                <button
                  key={f.user}
                  type="button"
                  className={cx('active-card', kind === 'expired' && 'expired')}
                  onClick={() => openModal({ kind: 'friendProfile', user: f.user })}
                  onContextMenu={(e) => openPersonMenu(e, f.user, f.profile.name)}
                >
                  <BannerBack user={f.user} banner={f.profile.banner} space={f.space} />
                  <Avatar id={f.user} name={f.profile.name} size={32} online={f.online} away={f.away} color={f.profile.theme_color} />
                  <span className="ac-names">
                    <span className="fr-name">{f.profile.name}</span>
                    <span className="muted small">
                      <Presence f={f} />
                      {kind === 'temporary' && ` · ${formatExpiresIn(f.expires_at)}`}
                    </span>
                    {/* 상대가 비공개로 했거나 기록이 없으면 데몬이 시각을 주지 않습니다. */}
                    {!f.online && f.last_seen !== undefined && <span className="muted small last-seen">{formatLastSeen(f.last_seen)}</span>}
                  </span>
                </button>
              ))
            )}
          </section>
        );
      })}
    </aside>
  );
}

export function HomeView() {
  const tab = useStore((s) => s.homeTab);
  const setTab = useStore((s) => s.setHomeTab);
  const friends = useStore((s) => s.friends);
  const requests = useStore((s) => s.requests);
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const refreshFriends = useStore((s) => s.refreshFriends);
  const refreshSpaces = useStore((s) => s.refreshSpaces);
  // 만료된 연락처는 연결하지 않으므로 온라인 목록에서 뺍니다.
  const online = friends.filter((f) => f.online && f.kind !== 'expired');

  const answer = async (id: string, accept: boolean) => {
    if (!client) return;
    const r = await call<unknown>(accept ? client.call('friend.accept', { request_id: id }) : client.call('friend.reject', { request_id: id }), {
      ok: accept ? t('친구가 됐습니다.') : undefined,
    });
    if (r !== undefined) {
      await refreshFriends();
      await refreshSpaces();
    }
  };

  const emptyHint = t('연락처 링크를 만들어 보내거나, 받은 링크를 붙여 넣으세요.');

  return (
    <main className="pane">
      <header className="topbar">
        <div className="tb-left">
          <BackButton />
          <Users size={22} className="muted" />
          <h1 className="tb-title">{t('친구')}</h1>
          <span className="tb-divider" />
          <nav className="tabs">
            <button type="button" className={cx('tab', tab === 'online' && 'active')} onClick={() => setTab('online')}>
              {t('온라인')}
            </button>
            <button type="button" className={cx('tab', tab === 'all' && 'active')} onClick={() => setTab('all')}>
              {t('모두')}
            </button>
            <button type="button" className={cx('tab', tab === 'pending' && 'active')} onClick={() => setTab('pending')}>
              {t('대기 중')} {requests.length > 0 && <span className="ch-badge">{requests.length}</span>}
            </button>
            <button type="button" className={cx('tab add', tab === 'add' && 'active')} onClick={() => setTab('add')}>
              {t('연락처 교환')}
            </button>
          </nav>
        </div>
      </header>
      <div className="home-body">
        {tab === 'add' ? (
          <AddFriendPanel />
        ) : tab === 'pending' ? (
          <div className="friend-list">
            <div className="list-head">
              {t('받은 친구 요청')} — {requests.length}
            </div>
            {requests.length === 0 && <Empty icon={<UserPlus size={36} />} title={t('대기 중인 요청이 없습니다')} />}
            {requests.map((r) => (
              <div key={r.request_id} className="friend-row">
                <Avatar id={r.user} name={r.profile.name} size={36} />
                <div className="fr-names">
                  <span className="fr-name">{r.profile.name}</span>
                  <span className="fr-status">
                    {r.profile.bio || t('친구 요청')} · {formatRelative(r.received_at)}
                  </span>
                </div>
                <button type="button" className="round-icon ok" title={t('수락')} onClick={() => void answer(r.request_id, true)}>
                  <Check size={18} />
                </button>
                <button type="button" className="round-icon danger" title={t('거절')} onClick={() => void answer(r.request_id, false)}>
                  <X size={18} />
                </button>
              </div>
            ))}
          </div>
        ) : tab === 'online' ? (
          <div className="friend-list">
            <div className="list-head">
              {t('온라인')} — {online.length}
            </div>
            {online.length === 0 && (
              <Empty icon={<Users size={36} />} title={t('지금 온라인인 친구가 없습니다')}>
                {emptyHint}
              </Empty>
            )}
            {online.map((f) => (
              <FriendRow key={f.user} f={f} />
            ))}
          </div>
        ) : (
          <div className="friend-list">
            {friends.length === 0 && (
              <Empty icon={<Users size={36} />} title={t('아직 친구가 없습니다')}>
                {emptyHint}
              </Empty>
            )}
            {KINDS.map((kind) => {
              const list = friends.filter((f) => f.kind === kind);
              if (list.length === 0) return null;
              return (
                <section key={kind} className="fl-section" aria-label={CONTACT_KIND_LABEL[kind]}>
                  <div className="list-head">
                    {CONTACT_KIND_LABEL[kind]} — {list.length}
                  </div>
                  {list.map((f) => (
                    <FriendRow key={f.user} f={f} />
                  ))}
                </section>
              );
            })}
          </div>
        )}
        <ContactSidePanel />
      </div>
    </main>
  );
}
