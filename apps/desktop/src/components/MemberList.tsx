// 오른쪽 멤버 목록: 순위 높은 역할별 → 온라인 → 최근 → 오프라인. 데몬이 있으면 `member.search`로 쪽마다 받아(3만 명 크루도
// 화면에 다 올리지 않음) 이름 검색·온라인·최근 거르기를 데몬에서 합니다. 데몬이 없거나 들어온 사람만 보이는 채널은
// 공간 보기의 멤버로 그립니다.

import { useEffect, useMemo, useState } from 'react';
import type { MemberFilter, MemberView, SpaceView } from '../api/types';
import { EVERYONE_ROLE } from '../api/types';
import { currentLang, t, tc } from '../i18n';
import { formatLastSeen } from '../lib/format';
import { byLevelDesc, cx, displayName, roleColor, topRole } from '../lib/misc';
import { useStore } from '../store/store';
import { BackButton } from './BackButton';
import { AnonIcon, Avatar, BannerBack, EmojiText } from './common';
import { openPersonMenu } from './menus';

const PAGE = 200;
/** 이보다 큰 크루는 처음에 온라인만 보여 줍니다. */
const BIG_CREW = 1000;
const RECENT_MS = 7 * 24 * 60 * 60 * 1000;

const FILTERS: Array<{ id: MemberFilter; label: string }> = [
  { id: 'online', label: t('온라인') },
  { id: 'recent', label: t('최근') },
  { id: 'all', label: t('전체') },
];

interface Result {
  members: MemberView[];
  total: number;
  online: number;
}

function matches(m: MemberView, q: string): boolean {
  return !q || displayName(m, '').toLowerCase().includes(q) || m.profile.name.toLowerCase().includes(q) || m.user.toLowerCase().startsWith(q);
}

export function MemberList({ space }: { space: SpaceView }) {
  const client = useStore((s) => s.client);
  const openModal = useStore((s) => s.openModal);
  const selected = useStore((s) => s.selected);
  const views = useStore((s) => s.views);
  const [query, setQuery] = useState('');
  const [filter, setFilter] = useState<MemberFilter>(() => (space.members.length > BIG_CREW ? 'online' : 'all'));
  const [remote, setRemote] = useState<Result | null>(null);
  const [more, setMore] = useState(false);
  // 차단 목록을 아는 동안은 그것을 기준으로(차단 직후 공간 보기를 다시 읽기 전에도 표시).
  const blockedList = useStore((s) => s.blocked);
  const blocked = useMemo(() => (blockedList ? new Set(blockedList.map((b) => b.user)) : null), [blockedList]);
  const channel = selected.space === space.space && selected.channel ? views[space.space]?.channels.find((c) => c.channel === selected.channel) : undefined;
  const joinLeave = channel?.settings.join_mode === 'join_leave';
  // 익명 채널은 데몬이 들어와 있는 사람을 수만 알려 줍니다(누구인지는 가림).
  const anonInChannel = joinLeave && !!channel?.anonymous;
  const useRemote = !!client && !joinLeave;
  const q = query.trim().toLowerCase();

  // 공간 보기가 바뀌면(접속·가입·역할) 첫 쪽을 다시 받습니다. 타자 중에는 잠깐 기다렸다가.
  useEffect(() => {
    if (!client || !useRemote) return;
    let stale = false;
    const timer = setTimeout(() => {
      client
        .call('member.search', { space: space.space, query: q || undefined, filter, limit: PAGE })
        .then((r) => !stale && setRemote(r))
        .catch(() => !stale && setRemote(null));
    }, 250);
    return () => {
      stale = true;
      clearTimeout(timer);
    };
  }, [client, useRemote, space.space, space.members, q, filter]);

  const loadMore = () => {
    if (!client || !remote || more) return;
    setMore(true);
    client
      .call('member.search', { space: space.space, query: q || undefined, filter, offset: remote.members.length, limit: PAGE })
      .then((r) => setRemote({ ...r, members: [...remote.members, ...r.members.filter((m) => !remote.members.some((x) => x.user === m.user))] }))
      .catch(() => undefined)
      .finally(() => setMore(false));
  };

  const local = useMemo((): Result => {
    const inChannel = joinLeave ? new Set(channel?.participants ?? []) : null;
    const all = space.members.filter((m) => !inChannel || inChannel.has(m.user));
    // 데몬 없이는 연결 기록을 몰라 "최근"은 온라인과 같습니다.
    const members = all.filter((m) => (filter === 'all' || m.online) && matches(m, q));
    return { members, total: members.length, online: all.filter((m) => m.online).length };
  }, [space.members, joinLeave, channel?.participants, filter, q]);

  const shown = useRemote && remote ? remote : local;

  const sections = useMemo(() => {
    const roles = space.roles.filter((r) => r.id !== EVERYONE_ROLE && r.rank > 0).sort(byLevelDesc);
    const byRole = new Map<string, MemberView[]>();
    const online: MemberView[] = [];
    const recent: MemberView[] = [];
    const offline: MemberView[] = [];
    const since = Date.now() - RECENT_MS;
    for (const m of shown.members) {
      if (!m.online) {
        (m.last_seen && m.last_seen >= since ? recent : offline).push(m);
        continue;
      }
      const top = topRole(m, roles);
      if (top) {
        const list = byRole.get(top.id) ?? [];
        list.push(m);
        byRole.set(top.id, list);
      } else online.push(m);
    }
    const sortByName = (a: MemberView, b: MemberView) => displayName(a).localeCompare(displayName(b), currentLang());
    const out: Array<{ id: string; label: string; members: MemberView[] }> = [];
    for (const r of roles) {
      const list = byRole.get(r.id);
      if (list?.length) out.push({ id: r.id, label: r.name, members: list.sort(sortByName) });
    }
    if (online.length) out.push({ id: 'online', label: t('온라인'), members: online.sort(sortByName) });
    // 최근은 데몬 순서(마지막 연결 최신 순) 그대로.
    if (recent.length) out.push({ id: 'recent', label: t('최근 7일'), members: recent });
    if (offline.length) out.push({ id: 'offline', label: t('오프라인'), members: offline.sort(sortByName) });
    return out;
  }, [shown.members, space.roles]);

  if (anonInChannel) {
    return (
      <aside className="members">
        <div className="mem-anon">
          <AnonIcon /> {t('익명 채널이라 들어와 있는 사람을 표시하지 않습니다({n}명)', { n: channel?.participants?.length ?? 0 })}
        </div>
      </aside>
    );
  }

  const rest = shown.total - shown.members.length;
  return (
    <aside className="members">
      <div className="mem-tools">
        <BackButton />
        <input className="mem-search" type="search" placeholder={t('멤버 검색')} aria-label={t('멤버 검색')} value={query} onChange={(e) => setQuery(e.target.value)} />
        <div className="mem-filters" role="group" aria-label={t('멤버 거르기')}>
          {FILTERS.filter((f) => useRemote || f.id !== 'recent').map((f) => (
            <button key={f.id} type="button" className={cx('chip', filter === f.id && 'active')} aria-pressed={filter === f.id} onClick={() => setFilter(f.id)}>
              {f.label}
            </button>
          ))}
        </div>
        <div className="mem-count">
          {t('온라인 {n}명', { n: shown.online })}
          {filter !== 'online' || q ? ' · ' + t('찾은 {n}명', { n: shown.total }) : ''}
        </div>
      </div>
      {joinLeave && <div className="mem-note">{t('이 채널에 들어와 있는 사람만 표시합니다')}</div>}
      {sections.length === 0 && <div className="mem-empty">{q ? t('찾는 멤버가 없습니다') : t('표시할 멤버가 없습니다')}</div>}
      {sections.map((sec) => (
        <div key={sec.id} className="mem-section">
          <div className="mem-head">
            <EmojiText text={sec.label} /> — {sec.members.length}
          </div>
          {sec.members.map((m) => {
            const role = topRole(m, space.roles);
            return (
              <button
                key={m.user}
                type="button"
                className={cx('mem-row', !m.online && 'off')}
                onClick={() => openModal({ kind: 'friendProfile', user: m.user })}
                onContextMenu={(e) => openPersonMenu(e, m.user, displayName(m))}
              >
                <BannerBack user={m.user} banner={m.profile.banner} space={space.space} />
                <Avatar id={m.user} name={displayName(m)} size={32} online={m.online} away={m.away} color={m.profile.theme_color} />
                <span className="mem-names">
                  <span className="mem-name" style={{ color: m.online ? roleColor(role?.color ?? 0) : undefined }}>
                    {displayName(m)}
                    {m.automation && <span className="tag bot">AI</span>}
                    {(blocked ? blocked.has(m.user) : m.blocked) && (
                      <span className="tag danger" title={t('내가 차단한 사람(이 기기에서만)')}>
                        {tc('차단한 사람 표시', '차단')}
                      </span>
                    )}
                    {/* 관리자를 볼 수 없는 사람에게는 소유자도 표시하지 않습니다. */}
                    {m.user === space.owner && !space.admins_hidden && (
                      <span className="crown" title={t('소유자')}>
                        ♛
                      </span>
                    )}
                  </span>
                  {m.online
                    ? m.profile.status && <span className="mem-status">{m.profile.status}</span>
                    : m.last_seen !== undefined && <span className="mem-status">{formatLastSeen(m.last_seen)}</span>}
                </span>
              </button>
            );
          })}
        </div>
      ))}
      {useRemote && rest > 0 && (
        <button type="button" className="link-btn" disabled={more} onClick={loadMore}>
          {t('{n}명 더 보기', { n: rest })}
        </button>
      )}
    </aside>
  );
}
