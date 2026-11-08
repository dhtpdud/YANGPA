// 채널 사이드바(크루·1대1)와 홈 사이드바(친구·연락처 교환·1대1 목록), 아래쪽 사용자 패널.

import {
  ArrowLeftRight,
  CheckCheck,
  ChevronDown,
  ChevronRight,
  FolderPlus,
  Gauge,
  Headphones,
  HeadphoneOff,
  Link2,
  LogOut,
  Mic,
  MicOff,
  PhoneOff,
  Plus,
  Settings,
  Signal,
  Users,
  Merge,
  Lock,
  Pencil,
  Handshake,
  IdCard,
  Trash2,
} from 'lucide-react';
import { useMemo, useState, type PointerEvent as ReactPointerEvent } from 'react';
import type { ChannelId, ChannelView, SpaceId, SpaceView } from '../api/types';
import { t, tc } from '../i18n';
import { ANON_NAME, anonParticipantLabel, isAnonymousUser } from '../lib/anon';
import type { DropWhere } from '../lib/channel';
import { formatDuration } from '../lib/format';
import { cx, displayName, hasPerm } from '../lib/misc';
import { isMuted, ruleKey } from '../lib/notify';
import { sortChannels, useStore } from '../store/store';
import { isPeerConnected } from '../voice/policy';
import { AnonIcon, Avatar, ChannelIcon, P2pIcon, Popover, Spinner } from './common';
import { MenuButton, MenuSep, NotifyItems, copyId, openPersonMenu } from './menus';
import { StatusMenu, myDot, myStatusText } from './StatusMenu';

/**
 * 커밋 대기(pending_commit): 멤버·기기 변경을 크루 키 그룹에 반영하는(키 갱신) 작업이 남음. 확정 기기만 할 수 있어서,
 * 이 기기가 확정 기기면 진행 중으로, 아니면 확정 기기를 기다리는 것으로 보입니다.
 * 키 회전 대기(key_rotation_pending): 나간 기기를 뺀 키 갱신이 오기 전에 옛 키로 보낸 글이 있어 그 기기도 읽을 수 있음.
 */
export function CommitNote({ space }: { space: SpaceView }) {
  const myDevice = useStore((s) => s.myDevice);
  if (space.key_rotation_pending) {
    return (
      <div
        className="sb-note"
        title={t('나간·내보낸 기기를 뺀 키 갱신이 아직 오지 않아 옛 키로 보냈습니다. 그 사이 보낸 글은 나간 기기도 읽을 수 있습니다. 크루 설정 → 개요 → 엄격한 키 회전을 켜면 갱신이 올 때까지 기다렸다 보냅니다.')}
      >
        {t('키 회전 대기 중: 나간 기기가 최근 글을 읽을 수 있습니다')}
      </div>
    );
  }
  if (!space.pending_commit) return null;
  const title = t('멤버·기기 변경을 크루 키 그룹에 반영하는 작업(키 갱신)이 남았습니다. 이 작업은 확정 기기(크루 설정 → 개요 → 확정 기기)만 할 수 있습니다.');
  return space.committer === myDevice ? (
    <div className="sb-note" title={title}>
      <Spinner size={12} /> {t('멤버·기기 변경을 키 그룹에 반영하는 중…')}
    </div>
  ) : (
    <div className="sb-note" title={title}>
      {t('확정 기기가 접속하면 멤버 변경이 반영됩니다')}
    </div>
  );
}

/**
 * 채널 오른쪽 클릭 메뉴: 읽음 표시, 초대, 알림 끄기·설정(크루의 글 채널, 이 기기 설정), 이 채널 속도 제한(누구나, 이 기기 설정),
 * 채널 설정(관리 권한), 채널 ID 복사. 휘발성 채널은 ID를 디스크·클립보드 기록에 남기지 않게 알림 규칙과 ID 복사를 뺍니다.
 */
function ChannelMenu({ space, c, anchor, onClose }: { space: SpaceView; c: ChannelView; anchor: DOMRect; onClose: () => void }) {
  const openModal = useStore((s) => s.openModal);
  const markRead = useStore((s) => s.markRead);
  const canManage = hasPerm(space, 'manage_channels');
  const crew = space.kind === 'crew';
  const volatile = c.settings.security === 'volatile';
  // 통화 채널은 통화 미디어를 세지 않고 카테고리는 내용이 없어 속도 제한을 걸 것이 없습니다.
  const limitable = c.settings.kind === 'text' || c.settings.kind === 'forum';
  const run = (fn: () => void) => () => {
    onClose();
    fn();
  };
  return (
    <Popover anchor={anchor} onClose={onClose} placement="bottom-start" className="menu wide">
      <MenuButton icon={<CheckCheck size={15} />} label={t('읽음으로 표시하기')} disabled={c.unread === 0} onClick={run(() => markRead(space.space, c.channel))} />
      {crew && hasPerm(space, 'create_invite') && (
        <>
          <MenuSep />
          <MenuButton icon={<Link2 size={15} />} label={t('크루에 초대하기')} onClick={run(() => openModal({ kind: 'crewSettings', space: space.space, tab: 'invites' }))} />
        </>
      )}
      {crew && limitable && !volatile && (
        <>
          <MenuSep />
          <NotifyItems space={space.space} channel={c.channel} onClose={onClose} />
        </>
      )}
      {(limitable || canManage) && <MenuSep />}
      {limitable && (
        <MenuButton
          icon={<Gauge size={15} />}
          label={t('이 채널 속도 제한')}
          onClick={run(() => openModal({ kind: 'rateLimit', scope: { kind: 'channel', space: space.space, channel: c.channel } }))}
        />
      )}
      {canManage && <MenuButton icon={<Settings size={15} />} label={t('채널 설정')} onClick={run(() => openModal({ kind: 'channelSettings', space: space.space, channel: c.channel }))} />}
      {!volatile && (
        <>
          <MenuSep />
          <MenuButton icon={<IdCard size={15} />} label={t('채널 ID 복사하기')} onClick={run(() => void copyId(c.channel, t('채널')))} />
        </>
      )}
    </Popover>
  );
}

type StartDrag = (e: ReactPointerEvent<HTMLElement>) => void;

export function ChannelRow({ space, c, active, drag }: { space: SpaceView; c: ChannelView; active: boolean; drag?: { start: StartDrag; className?: string } }) {
  const selectChannel = useStore((s) => s.selectChannel);
  const openModal = useStore((s) => s.openModal);
  const [menu, setMenu] = useState<DOMRect | null>(null);
  const roster = useStore((s) => s.voiceRosters[`${space.space}/${c.channel}`]);
  const members = space.members;
  const volatile = c.settings.security === 'volatile';
  // 이름 옆에 줄여 쓰는 수명: '30초', '퇴장'.
  const life = c.settings.ttl;
  const ttl = !volatile ? null : life.mode === 'seconds' ? formatDuration(life.seconds) : life.mode === 'on_leave' ? tc('수명 배지: 나가면 지움', '퇴장') : null;
  const canManage = hasPerm(space, 'manage_channels');
  // 알림을 끈 채널은 흐리게 그리고 안 읽음 표시를 하지 않습니다(디스코드처럼).
  const muted = useStore((s) => isMuted(s.prefs.notifyRules, ruleKey(space.space, c.channel)));
  const unread = (c.unread > 0 || !!c.has_new) && !active && !muted;
  const people = c.settings.kind === 'voice' || c.settings.join_mode === 'join_leave' ? c.participants ?? [] : [];
  // 익명 참가자는 모두 같은 자리 표시라 사람마다 줄을 만들고, 참가자 목록이 있으면 그 번호('익명 N')로 그립니다.
  const voiceRows =
    c.settings.kind !== 'voice'
      ? []
      : people.some(isAnonymousUser) && roster
        ? roster.participants.map((p) => ({ key: p.device, user: p.user, name: anonParticipantLabel(p) ?? ANON_NAME, p }))
        : people.map((u, i) => {
            const anon = isAnonymousUser(u);
            return {
              key: anon ? `anon-${i}` : u,
              user: u,
              name: anon ? ANON_NAME : displayName(members.find((x) => x.user === u)),
              p: anon ? undefined : roster?.participants.find((x) => x.user === u),
            };
          });
  return (
    <>
      <div
        data-ch={c.channel}
        className={cx('ch-row', active && 'active', unread && 'unread', muted && 'muted', volatile && 'volatile', drag?.className)}
        onPointerDown={drag?.start}
        onContextMenu={(e) => {
          e.preventDefault();
          // 누른 자리에 메뉴를 띄웁니다.
          setMenu(new DOMRect(e.clientX, e.clientY, 0, 0));
        }}
      >
        <button type="button" className="ch-btn" onClick={() => void selectChannel(space.space, c.channel)} title={c.settings.topic || c.settings.name}>
          <ChannelIcon channel={c} />
          {c.p2p && <P2pIcon className="ch-p2p" />}
          {c.anonymous && <AnonIcon className="ch-anon" />}
          <span className="ch-name">{c.settings.name}</span>
          {ttl && <span className="ch-ttl">{ttl}</span>}
          {c.settings.join_mode === 'join_leave' && (
            <span className={cx('ch-join', c.joined && 'on')} title={c.joined ? t('입장함') : t('입장하지 않음')}>
              {c.joined ? t('입장') : t('{n}명', { n: people.length })}
            </span>
          )}
        </button>
        {unread && c.unread > 0 && <span className="ch-badge">{c.unread > 99 ? '99+' : c.unread}</span>}
        {/* 아직 받지 않은 새 글: 수는 모르고 있다는 것만(열면 받아 옴). */}
        {unread && !c.unread && <span className="ch-new" title={t('새 글이 있습니다(열면 받아 옵니다)')} aria-label={t('새 글 있음')} />}
        {canManage && (
          <button type="button" className="ch-gear" title={t('채널 설정')} onClick={() => openModal({ kind: 'channelSettings', space: space.space, channel: c.channel })}>
            <Settings size={14} />
          </button>
        )}
      </div>
      {menu && <ChannelMenu space={space} c={c} anchor={menu} onClose={() => setMenu(null)} />}
      {voiceRows.length > 0 && (
        <div className="voice-people">
          {voiceRows.map(({ key, user, name, p }) => (
            <div key={key} className="vp-row" onContextMenu={(e) => openPersonMenu(e, user, name)}>
              <Avatar id={user} name={name} size={22} />
              <span className="vp-name">{name}</span>
              {p?.screen && <span className="live-mini">LIVE</span>}
              {p?.muted && <MicOff size={13} className="muted" />}
              {p?.deafened && <HeadphoneOff size={13} className="muted" />}
            </div>
          ))}
        </div>
      )}
    </>
  );
}

/**
 * 크루(1대1) 메뉴: 크루 목록 오른쪽 클릭과 사이드바 머리글. 읽음 표시, 초대, 알림 끄기·설정(크루만, 이 기기 설정),
 * 신뢰 스위치(크루만), 설정·만들기, 나가기(소유자는 해산), 크루 ID 복사.
 */
export function SpaceMenu({ space, anchor, onClose }: { space: SpaceView; anchor: DOMRect; onClose: () => void }) {
  const openModal = useStore((s) => s.openModal);
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const me = useStore((s) => s.me);
  const refreshSpaces = useStore((s) => s.refreshSpaces);
  const selectHome = useStore((s) => s.selectHome);
  const markRead = useStore((s) => s.markRead);
  const setTrust = useStore((s) => s.setTrust);
  const crew = space.kind === 'crew';
  const canManage = hasPerm(space, 'manage_channels');
  const run = (fn: () => void) => () => {
    onClose();
    fn();
  };
  const leave = () =>
    openModal({
      kind: 'confirm',
      title: t('크루 나가기'),
      body: t('「{name}」에서 나갑니다. 이 기기의 크루 키는 지워지지만, 다른 사람이 이미 받은 내 메시지는 남습니다.', { name: space.settings.name }),
      danger: true,
      confirmLabel: t('나가기'),
      onConfirm: async () => {
        if (!client) return;
        const ok = await call(client.call('crew.leave', { space: space.space }));
        if (ok !== undefined) {
          selectHome();
          await refreshSpaces();
        }
      },
    });
  const disband = () =>
    openModal({
      kind: 'confirm',
      title: t('크루 해산하기'),
      body: t('「{name}」 크루를 해산합니다. 모든 멤버에게서 크루가 끝나며 되돌릴 수 없습니다. 이 기기의 크루 데이터도 지웁니다.', { name: space.settings.name }),
      danger: true,
      confirmLabel: t('해산하기'),
      onConfirm: async () => {
        if (!client) return;
        const ok = await call(client.call('crew.disband', { space: space.space }));
        if (ok !== undefined) {
          selectHome();
          await refreshSpaces();
        }
      },
    });
  // 해산한 크루는 소유자도 나가기로 이 기기에서 지웁니다.
  const owner = space.owner === me && !space.disbanded;
  return (
    <Popover anchor={anchor} onClose={onClose} placement="bottom-start" className="menu wide">
      <MenuButton
        icon={<CheckCheck size={15} />}
        label={t('읽음으로 표시하기')}
        disabled={!space.channels.some((c) => c.unread > 0)}
        onClick={run(() => markRead(space.space))}
      />
      {crew && hasPerm(space, 'create_invite') && (
        <>
          <MenuSep />
          <MenuButton icon={<Link2 size={15} />} label={t('크루에 초대하기')} onClick={run(() => openModal({ kind: 'crewSettings', space: space.space, tab: 'invites' }))} />
        </>
      )}
      {crew && (
        <>
          <MenuSep />
          <NotifyItems space={space.space} onClose={onClose} />
        </>
      )}
      {/* 신뢰 스위치 전 데몬은 trusted를 보내지 않습니다. */}
      {crew && typeof space.trusted === 'boolean' && (
        <>
          <MenuSep />
          <MenuButton
            icon={<Handshake size={15} />}
            label={t('이 크루 신뢰')}
            sub={space.trusted ? t('멤버 모두에게 이 기기의 IP가 보임') : t('꺼짐: 이 기기의 IP를 숨김')}
            checked={space.trusted}
            // 켤 때 뜨는 경고 창과 Esc를 함께 받지 않게 메뉴부터 닫습니다.
            onClick={run(() => void setTrust({ type: 'space', space: space.space, name: space.settings.name }, !space.trusted))}
          />
        </>
      )}
      <MenuSep />
      {crew && <MenuButton icon={<Settings size={15} />} label={t('크루 설정')} onClick={run(() => openModal({ kind: 'crewSettings', space: space.space }))} />}
      {canManage && (
        <MenuButton icon={<FolderPlus size={15} />} label={t('카테고리 만들기')} onClick={run(() => openModal({ kind: 'channelCreate', space: space.space, channelKind: 'category' }))} />
      )}
      {canManage && <MenuButton icon={<Plus size={15} />} label={t('채널 만들기')} onClick={run(() => openModal({ kind: 'channelCreate', space: space.space }))} />}
      {crew && <MenuButton icon={<Pencil size={15} />} label={t('크루 별명 바꾸기')} onClick={run(() => openModal({ kind: 'crewSettings', space: space.space, tab: 'overview' }))} />}
      <MenuButton
        icon={<Gauge size={15} />}
        label={crew ? t('이 크루 속도 제한') : t('이 대화 속도 제한')}
        onClick={run(() => openModal({ kind: 'rateLimit', scope: { kind: 'crew', space: space.space } }))}
      />
      {!crew && !space.pending_crew_conversion && <MenuButton icon={<Merge size={15} />} label={t('크루로 전환')} onClick={run(() => openModal({ kind: 'convertCrew', space: space.space }))} />}
      {crew && (
        <>
          <MenuSep />
          {owner ? (
            <MenuButton icon={<Trash2 size={15} />} label={t('크루 해산하기')} danger onClick={run(disband)} />
          ) : (
            <MenuButton icon={<LogOut size={15} />} label={t('크루 나가기')} danger onClick={run(leave)} />
          )}
        </>
      )}
      {crew && (
        <>
          <MenuSep />
          <MenuButton icon={<IdCard size={15} />} label={t('크루 ID 복사하기')} onClick={run(() => void copyId(space.space, t('크루')))} />
        </>
      )}
    </Popover>
  );
}

type Drag = { id: ChannelId; drop: { target: ChannelId; where: DropWhere } | null };

/** 이만큼(px) 움직이기 전에는 끌기가 아니라 평소 클릭입니다. */
const DRAG_SLOP = 5;

/** 포인터 아래 놓을 자리. 카테고리는 카테고리 묶음 앞·뒤에만, 채널은 채널 줄 앞·뒤나 카테고리 머리글(안으로)에 놓습니다. */
function dropAt(x: number, y: number, category: boolean): Drag['drop'] {
  const el = document.elementFromPoint(x, y);
  const half = (e: HTMLElement): DropWhere => {
    const r = e.getBoundingClientRect();
    return y < r.top + r.height / 2 ? 'before' : 'after';
  };
  const cat = el?.closest<HTMLElement>('[data-cat]');
  const catId = cat?.dataset.cat;
  if (category) return cat && catId ? { target: catId, where: half(cat) } : null;
  const row = el?.closest<HTMLElement>('[data-ch]');
  if (row?.dataset.ch) return { target: row.dataset.ch, where: half(row) };
  return catId && el?.closest('.cat-head') ? { target: catId, where: 'into' } : null;
}

/**
 * 채널 관리 권한이 있으면 채널·카테고리를 끌어 옮깁니다. Tauri 파일 놓기(dragDropEnabled)가 Windows 웹뷰의 HTML5 끌어 놓기를
 * 가로채므로 포인터 이벤트로 처리합니다.
 */
function useChannelDrag(space: SpaceId, enabled: boolean) {
  const moveChannel = useStore((s) => s.moveChannel);
  const [drag, setDrag] = useState<Drag | null>(null);
  const start =
    (c: ChannelView): StartDrag =>
    (e) => {
      if (!enabled || e.button !== 0) return;
      const el = e.currentTarget;
      const { clientX: x0, clientY: y0, pointerId } = e;
      let cur: Drag | null = null;
      const move = (ev: PointerEvent) => {
        if (!cur) {
          if (Math.hypot(ev.clientX - x0, ev.clientY - y0) < DRAG_SLOP) return;
          // 창 밖에서 놓아도 끝나게 잡습니다. 누르자마자 잡으면 클릭이 단추 대신 줄로 가서 끌기가 시작된 뒤에 잡습니다.
          el.setPointerCapture?.(pointerId);
        }
        const found = dropAt(ev.clientX, ev.clientY, c.settings.kind === 'category');
        const drop = found?.target === c.channel ? null : found;
        // 놓을 자리가 그대로면 사이드바를 다시 그리지 않습니다(채널이 많은 크루에서 포인터 움직임마다 그리지 않게).
        if (cur && cur.drop?.target === drop?.target && cur.drop?.where === drop?.where) return;
        cur = { id: c.channel, drop };
        setDrag(cur);
      };
      const end = (ev: PointerEvent) => {
        window.removeEventListener('pointermove', move);
        window.removeEventListener('pointerup', end);
        window.removeEventListener('pointercancel', end);
        if (!cur) return;
        setDrag(null);
        // 끌기를 마친 자리의 클릭(채널 열기·카테고리 접기)은 먹습니다. 클릭이 오지 않을 때를 위해 곧바로 뗍니다.
        const eat = (ce: MouseEvent) => ce.stopPropagation();
        window.addEventListener('click', eat, { capture: true, once: true });
        setTimeout(() => window.removeEventListener('click', eat, true));
        if (ev.type === 'pointerup' && cur.drop) void moveChannel(space, c.channel, cur.drop.target, cur.drop.where);
      };
      window.addEventListener('pointermove', move);
      window.addEventListener('pointerup', end);
      window.addEventListener('pointercancel', end);
    };
  const className = (id: ChannelId) => cx(drag?.id === id && 'drag-src', drag?.drop?.target === id && `drop-${drag.drop.where}`);
  return { dragging: drag !== null, start, className };
}

export function ChannelSidebar({ space }: { space: SpaceView }) {
  const selected = useStore((s) => s.selected);
  const friends = useStore((s) => s.friends);
  const openModal = useStore((s) => s.openModal);
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>({});
  const [menu, setMenu] = useState<DOMRect | null>(null);
  const sorted = useMemo(() => sortChannels(space.channels), [space.channels]);
  const peer = space.kind === 'space' ? friends.find((f) => f.space === space.space) : undefined;
  const title = space.kind === 'space' ? peer?.profile.name ?? space.settings.name : space.settings.name;
  const canManage = hasPerm(space, 'manage_channels');
  const dnd = useChannelDrag(space.space, canManage);
  const rowDrag = (c: ChannelView) => (canManage ? { start: dnd.start(c), className: dnd.className(c.channel) } : undefined);

  const top = sorted.filter((c) => c.settings.kind !== 'category' && !c.settings.parent);
  const cats = sorted.filter((c) => c.settings.kind === 'category');

  return (
    <aside className="sidebar">
      <button type="button" className="sb-header" onClick={(e) => setMenu(e.currentTarget.getBoundingClientRect())}>
        <span className="sb-title">
          {space.kind === 'space' && <span className="tag">1:1</span>}
          {title}
        </span>
        <ChevronDown size={18} />
      </button>
      {menu && <SpaceMenu space={space} anchor={menu} onClose={() => setMenu(null)} />}
      <CommitNote space={space} />
      {space.dormant && <div className="sb-note">{t('휴면 공간: 열면 다시 따라잡습니다')}</div>}
      {space.disbanded && <div className="sb-note">{t('소유자가 해산한 크루입니다. 메뉴의 크루 나가기로 이 기기에서 지웁니다.')}</div>}
      <div className={cx('sb-scroll', dnd.dragging && 'dragging')}>
        {top.map((c) => (
          <ChannelRow key={c.channel} space={space} c={c} active={selected.channel === c.channel} drag={rowDrag(c)} />
        ))}
        {cats.map((cat) => {
          const kids = sorted.filter((c) => c.settings.parent === cat.channel);
          const isCollapsed = !!collapsed[cat.channel];
          return (
            <div key={cat.channel} data-cat={cat.channel} className={cx('sb-cat', canManage && dnd.className(cat.channel))}>
              <div className="cat-head" onPointerDown={canManage ? dnd.start(cat) : undefined}>
                <button type="button" className="cat-btn" onClick={() => setCollapsed({ ...collapsed, [cat.channel]: !isCollapsed })}>
                  {isCollapsed ? <ChevronRight size={12} /> : <ChevronDown size={12} />}
                  <span>{cat.settings.name}</span>
                </button>
                {canManage && (
                  <>
                    <button type="button" className="cat-add" title={t('카테고리 설정')} onClick={() => openModal({ kind: 'channelSettings', space: space.space, channel: cat.channel })}>
                      <Settings size={13} />
                    </button>
                    <button type="button" className="cat-add" title={t('채널 만들기')} onClick={() => openModal({ kind: 'channelCreate', space: space.space, parent: cat.channel })}>
                      <Plus size={15} />
                    </button>
                  </>
                )}
              </div>
              {kids
                .filter((c) => !isCollapsed || selected.channel === c.channel || c.unread > 0)
                .map((c) => (
                  <ChannelRow key={c.channel} space={space} c={c} active={selected.channel === c.channel} drag={rowDrag(c)} />
                ))}
            </div>
          );
        })}
      </div>
      <UserPanel />
    </aside>
  );
}

export function HomeSidebar() {
  const spaces = useStore((s) => s.spaces);
  const friends = useStore((s) => s.friends);
  const requests = useStore((s) => s.requests);
  const selectSpace = useStore((s) => s.selectSpace);
  const setHomeTab = useStore((s) => s.setHomeTab);
  const setMobilePane = useStore((s) => s.setMobilePane);
  const homeTab = useStore((s) => s.homeTab);
  const dms = spaces.filter((s) => s.kind === 'space');
  const openTab = (tab: 'online' | 'add') => {
    setHomeTab(tab);
    setMobilePane('chat');
  };
  return (
    <aside className="sidebar">
      <div className="sb-header static">
        <span className="sb-title">YANGPA</span>
      </div>
      <div className="sb-scroll">
        <button type="button" className={cx('home-link', homeTab !== 'add' && 'active')} onClick={() => openTab('online')}>
          <Users size={20} /> {t('친구')}
          {requests.length > 0 && <span className="ch-badge">{requests.length}</span>}
        </button>
        <button type="button" className={cx('home-link', homeTab === 'add' && 'active')} onClick={() => openTab('add')}>
          <ArrowLeftRight size={20} /> {t('연락처 교환')}
        </button>
        <div className="sb-section">{t('1대1 대화')}</div>
        {dms.map((s) => {
          const f = friends.find((x) => x.space === s.space);
          const name = f?.profile.name ?? s.name;
          const expired = f?.kind === 'expired';
          return (
            <button
              key={s.space}
              type="button"
              className={cx('dm-row', s.unread > 0 && 'unread', expired && 'expired')}
              title={expired ? t('만료된 연락처(읽기만)') : undefined}
              onClick={() => void selectSpace(s.space)}
              onContextMenu={(e) => f && openPersonMenu(e, f.user, name)}
            >
              <Avatar id={f?.user ?? s.space} name={name} size={32} online={f?.online} away={f?.away} color={f?.profile.theme_color} />
              <span className="dm-name">
                <span className="dm-title">
                  {name}
                  {f?.kind === 'temporary' && <span className="tag warn">{t('임시')}</span>}
                  {expired && <span className="tag">{t('만료')}</span>}
                </span>
                {f?.profile.status && <span className="dm-status">{f.profile.status}</span>}
              </span>
              {f?.safety_number_verified && (
                <span className="verified" title={t('안전 번호 확인함')}>
                  <Lock size={12} />
                </span>
              )}
              {s.unread > 0 && <span className="ch-badge">{s.unread}</span>}
            </button>
          );
        })}
        {dms.length === 0 && <div className="muted pad small">{t('연락처 링크로 대화를 시작하면 여기에 1대1 대화가 생깁니다.')}</div>}
      </div>
      <UserPanel />
    </aside>
  );
}

export function UserPanel() {
  const identity = useStore((s) => s.identity);
  const voice = useStore((s) => s.voice);
  const views = useStore((s) => s.views);
  const openModal = useStore((s) => s.openModal);
  const setMuted = useStore((s) => s.setMuted);
  const setDeafened = useStore((s) => s.setDeafened);
  const leaveVoice = useStore((s) => s.leaveVoice);
  const selectChannel = useStore((s) => s.selectChannel);
  const presence = useStore((s) => s.presence);
  const [statusMenu, setStatusMenu] = useState<DOMRect | null>(null);
  if (!identity) return null;
  const dot = myDot(presence);
  const vch = voice ? views[voice.space]?.channels.find((c) => c.channel === voice.channel) : undefined;
  const vspace = voice ? views[voice.space] : undefined;
  // 상대 모두가 실제로 이어져야(Tor는 첫 소리가 도착해야) 연결됨입니다. 아직 신호 전인 상대도 연결 중으로 셉니다.
  const connected = voice ? voice.participants.every((p) => p.device === identity.device || isPeerConnected(voice, p)) : false;
  return (
    <div className="user-panel">
      {voice && (
        <div className="up-voice">
          <button type="button" className="up-voice-info" onClick={() => void selectChannel(voice.space, voice.channel)}>
            <span className={cx('up-voice-state', connected ? 'ok' : 'wait')}>
              <Signal size={14} /> {connected ? t('통화 연결됨') : t('연결 중…')}
            </span>
            <span className="up-voice-ch">
              {vch?.settings.name ?? t('통화')} / {vspace?.kind === 'space' ? '1:1' : vspace?.settings.name}
            </span>
          </button>
          <button type="button" className="icon-btn" title={t('연결 끊기')} onClick={() => void leaveVoice()}>
            <PhoneOff size={18} />
          </button>
        </div>
      )}
      <div className="up-me">
        <button
          type="button"
          className="up-id"
          aria-haspopup="dialog"
          title={t('표시 상태·프로필')}
          onClick={(e) => setStatusMenu(e.currentTarget.getBoundingClientRect())}
        >
          <Avatar id={identity.user} name={identity.profile.name} size={32} online={dot !== 'off' && dot !== 'hidden'} away={dot === 'away'} color={identity.profile.theme_color} />
          <span className="up-names">
            <span className="up-name">{identity.profile.name}</span>
            <span className="up-status">{identity.profile.status || myStatusText(presence)}</span>
          </span>
        </button>
        {statusMenu && <StatusMenu anchor={statusMenu} onClose={() => setStatusMenu(null)} />}
        <button
          type="button"
          className={cx('icon-btn', voice?.muted && 'red')}
          title={voice ? (voice.muted ? t('마이크 켜기') : t('마이크 끄기')) : t('통화 중이 아닙니다')}
          disabled={!voice}
          onClick={() => voice && void setMuted(!voice.muted)}
        >
          {voice?.muted ? <MicOff size={18} /> : <Mic size={18} />}
        </button>
        <button
          type="button"
          className={cx('icon-btn', voice?.deafened && 'red')}
          title={voice ? (voice.deafened ? t('소리 켜기') : t('소리 끄기')) : t('통화 중이 아닙니다')}
          disabled={!voice}
          onClick={() => voice && void setDeafened(!voice.deafened)}
        >
          {voice?.deafened ? <HeadphoneOff size={18} /> : <Headphones size={18} />}
        </button>
        <button type="button" className="icon-btn" title={t('사용자 설정')} onClick={() => openModal({ kind: 'userSettings' })}>
          <Settings size={18} />
        </button>
      </div>
    </div>
  );
}
