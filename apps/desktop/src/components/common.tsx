// 작은 공용 컴포넌트: 아바타, 커스텀 이모지, 채널 아이콘(P2P 전용·익명 표시 포함), 팝오버, 토글, 스피너.

import { AlertTriangle, Flame, Hash, Lock, MessagesSquare, Volume2, Folder, UserPlus, UserRound, VenetianMask } from 'lucide-react';
import { memo, useEffect, useLayoutEffect, useRef, useState, type ReactNode, type SyntheticEvent } from 'react';
import { createPortal } from 'react-dom';
import type { ChannelView, EmojiView, FlairView, FriendView, QrCode, SpaceView } from '../api/types';
import { ANONYMOUS_USER, EVERYONE_ROLE } from '../api/types';
import { t } from '../i18n';
import { formatTtl } from '../lib/format';
import { ANON_NAME } from '../lib/anon';
import { parseEmojiTokens } from '../lib/emoji';
import { colorFromId, colorHex, cx, initials, roleColor } from '../lib/misc';
import { useStore, type Store } from '../store/store';

/**
 * 블롭 프로토콜 주소의 `key_from` 자리에 넣는 값(32바이트 0의 base58). 이모지·크루 아이콘·프로필 이미지는 데몬이 공간
 * 상태·서명된 프로필에서 키를 찾으므로 메시지 ID가 필요 없지만, 주소 형식은 유효한 이벤트 ID를 요구합니다.
 */
export const NO_KEY_EVENT = '1'.repeat(32);
/** 형식을 모르는 이미지(프로필·아이콘): 블롭 프로토콜이 바이트 앞부분으로 png·jpeg·gif·webp·avif를 가립니다. */
export const AUTO_IMAGE_MIME = 'auto';

type AvatarIndexInput = Pick<Store, 'views' | 'friends' | 'identity' | 'spaces'>;
let avatarMemo: { input: AvatarIndexInput; map: Map<string, string> } | null = null;

/**
 * 사용자 → `공간|블롭`. 데몬은 그 사람이 멤버인 공간을 통해서만 프로필 이미지를 내주므로(닫힌 스웜) 공간을 함께 고릅니다.
 * 상태가 바뀔 때만 다시 만듭니다(멤버 1000명 목록에서도 아바타마다 전체를 훑지 않게).
 */
export function avatarIndex(s: AvatarIndexInput): Map<string, string> {
  const m = avatarMemo;
  if (m && m.input.views === s.views && m.input.friends === s.friends && m.input.identity === s.identity && m.input.spaces === s.spaces) return m.map;
  const map = new Map<string, string>();
  for (const f of s.friends) if (f.profile.avatar) map.set(f.user, `${f.space}|${f.profile.avatar}`);
  for (const v of Object.values(s.views)) {
    for (const mem of v.members) if (mem.profile.avatar && !map.has(mem.user)) map.set(mem.user, `${v.space}|${mem.profile.avatar}`);
  }
  const me = s.identity;
  if (me?.profile.avatar) {
    // 내 이미지는 내가 멤버인 아무 공간으로 읽습니다.
    const space = Object.keys(s.views)[0] ?? s.friends[0]?.space ?? s.spaces[0]?.space;
    if (space) map.set(me.user, `${space}|${me.profile.avatar}`);
  }
  avatarMemo = { input: { views: s.views, friends: s.friends, identity: s.identity, spaces: s.spaces }, map };
  return map;
}

/** 못 읽어서 받아 달라고 한 이미지(공간|블롭). 같은 이미지를 여러 곳이 그려도 한 번만 요청합니다. */
const fetchRequested = new Set<string>();
const MAX_FETCH_REQUESTED = 4096;
/** 낮은 우선순위(데몬은 이것을 자동 받기 줄에 넣고 몇 번 실패하면 포기). 사용자가 누른 첨부 받기가 먼저 갑니다. */
const IMAGE_FETCH_PRIORITY = 64;

export function resetImageFetchForTest() {
  fetchRequested.clear();
}

/**
 * 공간 안 이미지 블롭(프로필·아이콘·이모지)의 주소. 데몬에 없으면(아직 못 받음) `<img>` 오류로 대신 표시하고, 데몬에
 * 받아 달라고 합니다(크루 멤버 사진은 데몬이 미리 받지 않음). 다 받으면(blob.progress) 다시 그립니다.
 */
export function useImageBlobSrc(space: string | undefined | null, blob: string | undefined | null, mime = AUTO_IMAGE_MIME): { src?: string; onError: () => void } {
  const client = useStore((s) => s.client);
  // 받기가 끝나면(blob.progress) 다시 시도합니다.
  const done = useStore((s) => (blob ? s.blobProgress[blob] : undefined));
  const ready = !done || done.have >= done.total;
  const key = `${space}|${blob}|${done?.have ?? ''}`;
  const [broken, setBroken] = useState<string | null>(null);
  if (!client || !space || !blob || !ready || broken === key) return { onError: () => undefined };
  const onError = () => {
    setBroken(key);
    const want = `${space}|${blob}`;
    if (fetchRequested.has(want) || fetchRequested.size >= MAX_FETCH_REQUESTED) return;
    fetchRequested.add(want);
    void client.call('blob.fetch', { space, blob, priority: IMAGE_FETCH_PRIORITY }).catch(() => fetchRequested.delete(want));
  };
  return { src: client.transport.blobSrc(space, blob, NO_KEY_EVENT, mime), onError };
}

/** 지금 보일 후원자 표시(기한이 지났으면 없음). */
export function liveFlair(f: FlairView | undefined): FlairView | undefined {
  return f && f.until > Date.now() ? f : undefined;
}

/**
 * 목록 카드 뒤에 까는 프로필 배너. 디스코드 멤버 목록처럼 왼쪽은 비우고 오른쪽으로 갈수록 진해집니다. 월 14달러 이상
 * 정기 후원자만 보입니다(프로필 창의 배너는 누구나). 배너는 아바타처럼 그 사람이 멤버인 공간을 통해 읽고, 모르면
 * `space`로 읽습니다.
 */
export function BannerBack({ user, banner, space }: { user: string; banner: string | null; space?: string }) {
  const shown = useStore((s) => (banner && liveFlair(s.flair[user])?.banner ? banner : null));
  const where = useStore((s) => (shown ? avatarIndex(s).get(user)?.split('|')[0] : undefined));
  const img = useImageBlobSrc(where ?? space, shown);
  return img.src ? <StillImg className="banner-back" src={img.src} onError={img.onError} /> : null;
}

/**
 * 프로필 사진·배너. 움짤(gif·webp)은 첫 장에 멈춰 두고, 그 사진이나 프로필 카드 위에 마우스를 올릴 때만 재생합니다
 * (CSS `.still`·`.play`). 멈춘 장은 불러온 `<img>`를 캔버스에 한 번 그린 것이라 정지 이미지도 똑같이 보입니다.
 */
export function StillImg({ className, src, onError }: { className: string; src: string; onError: () => void }) {
  const still = useRef<HTMLCanvasElement>(null);
  const draw = (e: SyntheticEvent<HTMLImageElement>) => {
    const img = e.currentTarget;
    const c = still.current;
    if (!c || !img.naturalWidth || !img.naturalHeight) return;
    // 보이는 크기(cover)만큼만 그립니다: 큰 배너를 원본 크기로 줄마다 들고 있지 않게.
    const dpr = window.devicePixelRatio || 1;
    const scale = Math.min(1, Math.max((c.clientWidth * dpr) / img.naturalWidth, (c.clientHeight * dpr) / img.naturalHeight) || 1);
    c.width = Math.max(1, Math.round(img.naturalWidth * scale));
    c.height = Math.max(1, Math.round(img.naturalHeight * scale));
    c.getContext('2d')?.drawImage(img, 0, 0, c.width, c.height);
  };
  return (
    <>
      <canvas ref={still} className={cx(className, 'still')} aria-hidden />
      <img className={cx(className, 'play')} src={src} alt="" decoding="async" draggable={false} onLoad={draw} onError={onError} />
    </>
  );
}

export function Avatar({
  id,
  name,
  size = 40,
  online,
  away,
  color,
  speaking,
  className,
  halo: haloOverride,
}: {
  id: string;
  name: string;
  size?: number;
  online?: boolean;
  /** 온라인이지만 자리 비움(노란 점). */
  away?: boolean;
  color?: number | null;
  speaking?: boolean;
  className?: string;
  /** 후광 색 미리보기(설정 화면). 주지 않으면 그 사람이 낸 후원자 표시를 따릅니다. */
  halo?: [number, number] | null;
}) {
  const anon = id === ANONYMOUS_USER;
  const bg = color && !anon ? roleColor(color) : colorFromId(id);
  // 익명 자리는 누구의 사진도 찾지 않고 기본 그림으로 그립니다. 후광도 없습니다(색으로 누군지 드러나지 않게).
  const where = useStore((s) => (anon ? undefined : avatarIndex(s).get(id)));
  const shownHalo = useStore((s) => (anon ? undefined : liveFlair(s.flair[id])?.halo));
  const halo = anon ? undefined : haloOverride === undefined ? shownHalo : haloOverride;
  const [space, blob] = where ? where.split('|') : [];
  const img = useImageBlobSrc(space, blob);
  const anonBg = name && name !== ANON_NAME ? colorFromId(name) : undefined;
  return (
    <span
      className={cx('avatar', anon && 'anon', speaking && 'speaking', img.src && 'has-img', halo && 'halo', className)}
      // 익명 자리는 사진 없이, 가명이 있으면 가명마다 다른 바탕색으로 구분합니다. 후광이 있으면 바탕은 후광 위 원(::after)이 그립니다.
      style={{
        width: size,
        height: size,
        fontSize: Math.max(10, Math.round(size * 0.38)),
        background: anon ? anonBg : bg,
        ...(halo && { ['--halo-a' as string]: colorHex(halo[0]), ['--halo-b' as string]: colorHex(halo[1]), ['--av-bg' as string]: img.src ? 'transparent' : bg }),
      }}
      aria-hidden
    >
      {anon ? (
        <UserRound size={Math.round(size * 0.6)} />
      ) : img.src ? (
        <StillImg className="avatar-img" src={img.src} onError={img.onError} />
      ) : size < 28 ? (
        Array.from(initials(name))[0]
      ) : (
        initials(name)
      )}
      {online !== undefined && <span className={cx('presence', online ? (away ? 'away' : 'on') : 'off')} />}
    </span>
  );
}

/** 크루 아이콘(없으면 이름 머리글자). */
export function SpaceIcon({ space, icon, name, size = 48, className }: { space: string; icon?: string | null; name: string; size?: number; className?: string }) {
  const img = useImageBlobSrc(space, icon ?? null);
  return img.src ? (
    <img className={cx('space-icon', className)} src={img.src} alt="" width={size} height={size} decoding="async" draggable={false} onError={img.onError} />
  ) : (
    <span className={cx('space-icon initials', className)}>{initials(name)}</span>
  );
}

/** 모든 공간에서 이모지 ID로 찾습니다(다른 크루 이모지도 보이게). 상태 안 객체를 그대로 돌려줘 참조가 안정적입니다. */
function findEmoji(views: Record<string, SpaceView>, id: string): EmojiView | undefined {
  for (const v of Object.values(views)) {
    const e = v.emojis.find((x) => x.id === id);
    if (e) return e;
  }
  return undefined;
}

function findEmojiSpace(views: Record<string, SpaceView>, id: string): string | undefined {
  for (const v of Object.values(views)) if (v.emojis.some((x) => x.id === id)) return v.space;
  return undefined;
}

export const CustomEmoji = memo(function CustomEmoji({
  id,
  name,
  size = 22,
}: {
  id: string;
  name: string;
  animated?: boolean;
  size?: number;
}) {
  const emoji = useStore((s) => findEmoji(s.views, id));
  const space = useStore((s) => findEmojiSpace(s.views, id));
  // 키는 데몬이 공간 상태(CustomEmoji::key)에서 찾습니다. 형식은 EmojiView.mime(비었으면 바이트로 가림).
  const img = useImageBlobSrc(space, emoji?.blob, emoji?.mime || AUTO_IMAGE_MIME);
  if (!emoji || !img.src) {
    return (
      <span className="emoji-fallback" title={`:${name}:`}>
        :{name}:
      </span>
    );
  }
  return (
    <img
      className="emoji"
      src={img.src}
      alt={`:${name}:`}
      title={`:${name}:`}
      width={size}
      height={size}
      decoding="async"
      draggable={false}
      onError={img.onError}
    />
  );
});

/** 커스텀 이모지 표기(`<:이름:ID>`)를 그림으로 바꿔 그리는 짧은 글(역할 이름 등). 그림 크기는 글자 크기를 따릅니다. */
export function EmojiText({ text }: { text: string }) {
  const toks = parseEmojiTokens(text);
  if (!toks.length) return <>{text}</>;
  const out: ReactNode[] = [];
  let at = 0;
  toks.forEach((tok, i) => {
    if (tok.start > at) out.push(text.slice(at, tok.start));
    out.push(<CustomEmoji key={i} id={tok.id} name={tok.name} animated={tok.animated} />);
    at = tok.end;
  });
  if (at < text.length) out.push(text.slice(at));
  return <span className="emoji-text">{out}</span>;
}

export function isPrivateChannel(c: ChannelView): boolean {
  return c.settings.overwrites.some((o) => o.target.type === 'role' && o.target.id === EVERYONE_ROLE && (o.deny & 1) === 1);
}

export function ChannelIcon({ channel, size = 18 }: { channel: ChannelView; size?: number }) {
  const s = channel.settings;
  const kind = s.kind;
  const volatile = s.security === 'volatile';
  let icon: ReactNode;
  if (kind === 'voice') icon = <Volume2 size={size} />;
  else if (kind === 'forum') icon = <MessagesSquare size={size} />;
  else if (kind === 'category') icon = <Folder size={size} />;
  else if (volatile) icon = <Flame size={size} />;
  else icon = <Hash size={size} />;
  return (
    <span className={cx('ch-icon', volatile && 'volatile')}>
      {icon}
      {isPrivateChannel(channel) && <Lock className="ch-lock" size={10} strokeWidth={3} />}
    </span>
  );
}

/** P2P 전용 채널 표시(점 둘을 바로 잇는 선: Tor 회로 없이 기기끼리 직접). */
export function P2pIcon({ size = 14, className }: { size?: number; className?: string }) {
  return (
    <span className={cx('p2p-icon', className)} role="img" aria-label={t('P2P 전용 채널')} title={t('P2P 전용 채널')}>
      <svg viewBox="0 0 16 16" width={size} height={size} aria-hidden>
        <circle cx="3.5" cy="8" r="2.5" fill="currentColor" />
        <circle cx="12.5" cy="8" r="2.5" fill="currentColor" />
        <path d="M6 8h4" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
      </svg>
    </span>
  );
}

/** 익명 채널 표시(가면). */
export function AnonIcon({ size = 14, className }: { size?: number; className?: string }) {
  return (
    <span className={cx('anon-icon', className)} role="img" aria-label={t('익명 채널')} title={t("익명 채널: 글쓴이와 통화 참가자가 '익명'으로 보입니다")}>
      <VenetianMask size={size} aria-hidden />
    </span>
  );
}

/** 휘발성 배지: 불꽃 + 수명(TTL) */
export function VolatileBadge({ channel, compact }: { channel: ChannelView; compact?: boolean }) {
  const s = channel.settings;
  if (s.security !== 'volatile') return null;
  const ttl = formatTtl(s.ttl);
  return (
    <span className="badge volatile" title={t('휘발성 채널: 디스크에 저장하지 않고 수명이 지나면 지워집니다')}>
      <Flame size={11} strokeWidth={2.5} />
      {!compact && <span>{t('휘발성')}</span>}
      {ttl && <span className="ttl">{ttl}</span>}
    </span>
  );
}

export function Spinner({ size = 16 }: { size?: number }) {
  return <span className="spinner" style={{ width: size, height: size }} aria-label={t('불러오는 중')} />;
}

export function Toggle({
  checked,
  onChange,
  disabled,
  label,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  disabled?: boolean;
  label?: string;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      disabled={disabled}
      className={cx('toggle', checked && 'on')}
      onClick={() => onChange(!checked)}
    >
      <span className="knob" />
    </button>
  );
}

/**
 * 고정 위치 팝오버. 기준 사각형 옆에 띄우고 화면 밖으로 나가지 않게 맞춥니다.
 * 바깥 클릭·Esc로 닫힙니다.
 */
export function Popover({
  anchor,
  onClose,
  children,
  placement = 'top-end',
  className,
}: {
  anchor: DOMRect;
  onClose: () => void;
  children: ReactNode;
  placement?: 'top-end' | 'bottom-start' | 'right-start' | 'top-start';
  className?: string;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const w = el.offsetWidth;
    const h = el.offsetHeight;
    let left = anchor.right - w;
    let top = anchor.top - h - 6;
    if (placement === 'bottom-start') {
      left = anchor.left;
      top = anchor.bottom + 6;
    } else if (placement === 'right-start') {
      left = anchor.right + 8;
      top = anchor.top;
    } else if (placement === 'top-start') {
      left = anchor.left;
      top = anchor.top - h - 6;
    }
    if (top < 8) top = Math.min(anchor.bottom + 6, window.innerHeight - h - 8);
    left = Math.max(8, Math.min(left, window.innerWidth - w - 8));
    top = Math.max(8, Math.min(top, window.innerHeight - h - 8));
    setPos({ left, top });
  }, [anchor, placement]);
  useEffect(() => {
    const onDown = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    const timer = setTimeout(() => document.addEventListener('mousedown', onDown), 0);
    document.addEventListener('keydown', onKey);
    return () => {
      clearTimeout(timer);
      document.removeEventListener('mousedown', onDown);
      document.removeEventListener('keydown', onKey);
    };
  }, [onClose]);
  return createPortal(
    <div
      ref={ref}
      className={cx('popover', className)}
      style={pos ? { left: pos.left, top: pos.top } : { left: -9999, top: -9999 }}
      role="dialog"
    >
      {children}
    </div>,
    document.body,
  );
}

export function Empty({ icon, title, children }: { icon?: ReactNode; title: string; children?: ReactNode }) {
  return (
    <div className="empty">
      {icon && <div className="empty-icon">{icon}</div>}
      <div className="empty-title">{title}</div>
      {children && <div className="empty-body">{children}</div>}
    </div>
  );
}

/** 클립보드 복사(실패해도 조용히). */
export async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}

/**
 * 기기 연결 링크처럼 남으면 안 되는 글 복사. 데스크톱 앱은 클립보드 기록·클라우드 클립보드에서 빼고 1분 뒤 비우고,
 * 못 하면 웹뷰가 그냥 복사합니다. 결과는 알림으로 알립니다.
 */
export async function copySecret(text: string): Promise<void> {
  const { client, toast } = useStore.getState();
  const guarded = await client?.transport.secret?.copy(text).catch(() => false);
  if (guarded) toast('success', t('복사했습니다. 1분 뒤 클립보드에서 지웁니다(클립보드 기록에도 남지 않음).'));
  else if (await copyText(text)) toast('success', t('복사했습니다. 붙여 넣은 뒤 클립보드를 비워 두세요.'));
  else toast('error', t('복사하지 못했습니다'));
}

/** 방금 만든 링크에 찾아올 주소가 없을 때(`linkLacksAddress`). 링크는 그대로 두되 그 링크로는 연결되지 않음을 알립니다. */
export function NoAddressWarning() {
  return (
    <div className="warn-box small" role="note">
      <AlertTriangle size={16} />
      <div>
        {t(
          '이 링크에는 아직 다른 네트워크에서 찾아올 주소가 없습니다. 이 데몬은 Tor 없이 직접 연결로 떠 있고 중계 노드에 연결되기 전이라, 같은 네트워크(Wi-Fi)에 있지 않은 사람은 이 링크로 연결할 수 없습니다. 잠시 뒤 링크를 다시 만드세요.',
        )}
      </div>
    </div>
  );
}

/** 임시 연락처의 친구 추가(`friend.propose`). 보낸 뒤에는 상대 수락을 기다리는 동안 '요청 보냄'으로 잠깁니다. */
export function ProposeFriendButton({ friend }: { friend: FriendView }) {
  const proposeFriend = useStore((s) => s.proposeFriend);
  const [busy, setBusy] = useState(false);
  if (friend.kind !== 'temporary') return null;
  if (friend.proposed) {
    return (
      <button type="button" className="btn small" disabled title={t('상대가 수락하면 친구가 됩니다')}>
        {t('요청 보냄')}
      </button>
    );
  }
  return (
    <button
      type="button"
      className="btn small primary"
      disabled={busy}
      onClick={async () => {
        setBusy(true);
        await proposeFriend(friend.user);
        setBusy(false);
      }}
    >
      <UserPlus size={14} /> {t('친구 추가')}
    </button>
  );
}

/** QR 칸들을 SVG 사각형으로(raw HTML 없이). 둘레 4칸은 카메라가 읽도록 흰 여백. */
export function QrImage({ qr, label }: { qr: QrCode; label: string }) {
  const quiet = 4;
  const n = qr.size + quiet * 2;
  let d = '';
  for (let i = 0; i < qr.size * qr.size; i++) {
    if (qr.modules[i] === '1') d += `M${(i % qr.size) + quiet} ${Math.floor(i / qr.size) + quiet}h1v1h-1z`;
  }
  return (
    <svg className="qr-code" viewBox={`0 0 ${n} ${n}`} width={200} height={200} shapeRendering="crispEdges" role="img" aria-label={label}>
      <rect width={n} height={n} fill="#fff" />
      <path d={d} fill="#000" />
    </svg>
  );
}
