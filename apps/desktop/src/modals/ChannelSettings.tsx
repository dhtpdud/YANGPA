// 채널·카테고리 만들기·설정. 일반: 종류, 보안 등급(일반·휘발성), 입장 방식, 수명(TTL), 과거 기록, 링크 미리보기, P2P 전용, 익명.
// 권한: 비공개 스위치와 역할·멤버별 덮어쓰기(ChannelPerms). P2P 전용은 만들 때는 폼과 함께 보내고, 만든 뒤에는 저장과 따로 확인을
// 거쳐 `channel.set_p2p`로 바꿉니다. 익명도 만들 때는 폼과 함께, 만든 뒤에는 스위치를 누르는 즉시 따로 보냅니다(통화 채널을 끌 때만
// 확인). 카테고리 권한은 데몬이 물려주지 않으므로, 카테고리와 같던 채널을 앱이 함께 맞춥니다.
// 포럼 채널은 그 채널의 채널 관리 권한이 있으면 태그(글자 하나씩, 20개까지)를 더하고 고치고 지웁니다(저장할 때 목록 전체).
// 크루 채널은 채널 관리 권한으로 첨부 하나의 용량 한도(입력란: 비우면 무제한, 0은 첨부 금지, 휘발성은 기본 10MiB)와
// 수정·삭제 금지를 정합니다(`attach_limit`·`no_edit`·`no_delete`). 저장해도 창은 닫지 않습니다(만들기만 닫음).

import { AlertTriangle, Flame, Hash, Link2, MessagesSquare, Trash2, Unlink, Volume2 } from 'lucide-react';
import { useMemo, useState, type ReactNode } from 'react';
import type { ChannelKind, ChatTtl, ForumTag, HistoryVisibility, JoinMode, PermissionOverwrite, SecurityLevel } from '../api/types';
import { AnonIcon, P2pIcon, Toggle } from '../components/common';
import { t, tc } from '../i18n';
import { ANON_HINT, canSetAnonymous } from '../lib/anon';
import {
  TTL_MAX_SECS,
  TTL_MIN_SECS,
  VOLATILE_ATTACH_DEFAULT,
  VOLATILE_CAP_DEFAULT,
  attachLimitMib,
  canBeP2p,
  canHaveAttachLimit,
  channelCreateParams,
  channelUpdatePatch,
  clampTtl,
  hasChatLifetime,
  parseAttachLimit,
  parseVolatileCap,
  volatileLimitTooBig,
  type ChannelForm,
} from '../lib/channel';
import { formatDuration } from '../lib/format';
import { tagsError } from '../lib/forumTags';
import { PERMISSION_LABELS, canGrant, cx } from '../lib/misc';
import { compactOverwrites, hasChannelPerm, liveOverwrites, overwriteDelta, sameOverwrites, syncedChildren } from '../lib/perms';
import { useStore } from '../store/store';
import { ChannelPermsEditor } from './ChannelPerms';
import { ForumTagsEditor } from './ForumTags';
import { Field, Modal } from './Modal';

type TtlChoice = 'none' | 'on_leave' | 'seconds';

const VOLATILE_CAP_HINT = t(
  '동시에 참여하는 기기 수의 상한입니다. 넘으면 일부 기기는 자리가 날 때까지 보고 쓰지 못합니다. 클수록 나갈 때마다 키를 다시 나눠 주는 비용이 커집니다.',
);

const TTL_PRESETS = [30, 60, 300, 3600, 86_400, 7 * 86_400];

/** 휘발성 채널을 만들 때 넣어 두는 한도(MiB). */
const VOLATILE_START_MIB = attachLimitMib(VOLATILE_ATTACH_DEFAULT);
export const ATTACH_LIMIT_HINT = t('멤버가 이 채널에 올리는 파일 하나의 최대 크기(MiB)입니다. 비우면 무제한, 0이면 첨부 금지입니다.');
// 앞 문장 뒤에 공백 하나를 두고 이어 붙입니다(키에 앞 공백이 끼지 않게 상수에서는 뺌).
export const VOLATILE_LIMIT_HINT = t('휘발성 채널의 첨부는 각 기기의 메모리 예산(기본 1GiB)도 넘을 수 없습니다.');
export const VOLATILE_RAM_WARNING = t('휘발성 첨부는 채널 참가자 모두의 RAM에 올라갑니다. {n}MiB보다 크게 두려면 참가자 기기의 RAM 용량을 고려하세요.', {
  n: VOLATILE_START_MIB,
});
export const NO_EDIT_HINT = t('켜면 아무도 이 채널의 메시지(포럼 글 본문 포함)를 고칠 수 없습니다. 포럼 글 제목·태그는 메시지 관리 권한이 있어야 고칩니다.');
export const NO_DELETE_HINT = t('켜면 글쓴이가 자기 메시지를 지울 수 없습니다. 메시지 관리 권한이 있는 운영진은 지울 수 있고, 수명(TTL) 만료는 그대로입니다.');

const KINDS: Array<{ kind: ChannelKind; label: string; desc: string; icon: ReactNode }> = [
  { kind: 'text', label: t('텍스트'), desc: t('메시지·이미지·이모지'), icon: <Hash size={22} /> },
  { kind: 'voice', label: t('통화'), desc: t('음성 대화·화면공유'), icon: <Volume2 size={22} /> },
  { kind: 'forum', label: t('포럼'), desc: t('주제별 글과 답글'), icon: <MessagesSquare size={22} /> },
];

export function ChannelSettingsModal({
  space,
  channel,
  parent,
  initialKind,
  onClose,
}: {
  space: string;
  channel?: string;
  parent?: string | null;
  initialKind?: ChannelKind;
  onClose: () => void;
}) {
  const view = useStore((s) => s.views[space]);
  const me = useStore((s) => s.me);
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const openModal = useStore((s) => s.openModal);
  const closeModal = useStore((s) => s.closeModal);
  const selectChannel = useStore((s) => s.selectChannel);
  const setChannelP2p = useStore((s) => s.setChannelP2p);
  const setChannelAnonymous = useStore((s) => s.setChannelAnonymous);
  const toast = useStore((s) => s.toast);
  const existing = channel ? view?.channels.find((c) => c.channel === channel) : undefined;
  const s0 = existing?.settings;
  const members = view?.members;
  const channels = view?.channels;

  // 나간 멤버의 덮어쓰기는 뺀 값을 기준으로 삼습니다(데몬이 멤버가 아닌 대상을 거절하므로).
  const overwrites0 = useMemo(() => (s0 ? liveOverwrites(s0.overwrites, members ?? []) : null), [s0, members]);
  const categoryOverwrites = (id: string | null | undefined): PermissionOverwrite[] | null => {
    const c = id ? channels?.find((x) => x.channel === id && x.settings.kind === 'category') : undefined;
    return c ? liveOverwrites(c.settings.overwrites, members ?? []) : null;
  };

  const [kind, setKind] = useState<ChannelKind>(s0?.kind ?? initialKind ?? 'text');
  const [name, setName] = useState(s0?.name ?? '');
  const [topic, setTopic] = useState(s0?.topic ?? '');
  const [security, setSecurity] = useState<SecurityLevel>(s0?.security ?? 'normal');
  const [joinMode, setJoinMode] = useState<JoinMode>(s0?.join_mode ?? 'always');
  const [ttl, setTtl] = useState<TtlChoice>(s0?.ttl.mode ?? 'none');
  const [ttlSecs, setTtlSecs] = useState<number>(s0?.ttl.mode === 'seconds' ? s0.ttl.seconds : 300);
  const [history, setHistory] = useState<HistoryVisibility>(s0?.history ?? 'private');
  const [linkPreviews, setLinkPreviews] = useState<boolean>(s0?.link_previews ?? true);
  const [parentId, setParentId] = useState<string | null>(s0?.parent ?? parent ?? null);
  // 카테고리 안에 만들면 카테고리 권한에서 시작합니다.
  const [overwrites, setOverwrites] = useState<PermissionOverwrite[]>(() => overwrites0 ?? categoryOverwrites(parent) ?? []);
  const [permsEdited, setPermsEdited] = useState(false);
  const [tab, setTab] = useState<'general' | 'perms'>('general');
  const [p2p, setP2p] = useState(false);
  const [anonymous, setAnonymous] = useState(false);
  const [forumTags, setForumTags] = useState<ForumTag[]>(() => s0?.forum_tags ?? []);
  // 입력란의 MiB 글자(비우면 무제한, 0은 첨부 금지).
  const [limitText, setLimitText] = useState<string>(existing?.attach_limit != null ? attachLimitMib(existing.attach_limit) : '');
  const [limitTouched, setLimitTouched] = useState(false);
  const [noEdit, setNoEdit] = useState(!!existing?.no_edit);
  const [noDelete, setNoDelete] = useState(!!existing?.no_delete);
  const [capText, setCapText] = useState<string>(String(existing?.volatile_cap ?? VOLATILE_CAP_DEFAULT));
  const [busy, setBusy] = useState(false);

  if (!view) return null;
  const volatile = security === 'volatile';
  const isText = kind === 'text';
  const lifetime = hasChatLifetime(kind);
  const p2pAllowed = canBeP2p(kind, view.kind);
  const anonAllowed = canSetAnonymous(view, kind);
  // 만들 때는 크루 권한, 고칠 때는 그 채널의 덮어쓰기까지 본 채널 관리 권한.
  const canEditTags = kind === 'forum' && hasChannelPerm(view, existing, me, 'manage_channels');
  const tagError = canEditTags ? tagsError(forumTags.map((tag) => ({ ...tag, name: tag.name.trim() }))) : null;
  // 첨부 한도·수정·삭제 금지도 태그처럼 채널 관리 권한으로 정합니다.
  const canEditRules = canHaveAttachLimit(kind, view.kind) && hasChannelPerm(view, existing, me, 'manage_channels');
  const limitParsed = parseAttachLimit(limitText);
  const limitBytes = limitParsed.ok ? limitParsed.bytes : null;
  const limitError = canEditRules && !limitParsed.ok ? limitParsed.error : null;
  const ramWarning = canEditRules && volatile && limitParsed.ok && volatileLimitTooBig(limitBytes);
  // 참가 상한은 이미 있는 상시 휘발성 채널에서만 바꿉니다(만들 때는 기본 200).
  const canEditCap = canEditRules && !!existing && volatile && joinMode === 'always';
  const capParsed = parseVolatileCap(capText);
  const capError = canEditCap && !capParsed.ok ? capParsed.error : null;
  const chatTtl: ChatTtl = ttl === 'seconds' ? { mode: 'seconds', seconds: clampTtl(ttlSecs) } : { mode: ttl };
  const form: ChannelForm = {
    name,
    topic,
    parent: kind === 'category' ? null : parentId,
    joinMode,
    ttl: chatTtl,
    history,
    linkPreviews,
    overwrites,
    p2p,
    anonymous: anonAllowed && anonymous,
    forumTags: canEditTags ? forumTags : undefined,
    attachLimit: canEditRules && !limitError ? limitBytes : undefined,
    noEdit: canEditRules ? noEdit : undefined,
    noDelete: canEditRules ? noDelete : undefined,
    volatileCap: canEditCap && capParsed.ok ? capParsed.cap : undefined,
  };
  const categories = view.channels.filter((c) => c.settings.kind === 'category');
  const sendsOverwrites = overwrites0 ? !sameOverwrites(overwrites, overwrites0) : compactOverwrites(overwrites).length > 0;
  // 데몬처럼 바뀌는 권한 비트가 모두 내게 있어야 저장됩니다(소유자·관리자는 전부).
  const blocked = sendsOverwrites ? overwriteDelta(s0?.overwrites ?? [], overwrites).filter((p) => !canGrant(view, p)) : [];
  const parentCat = kind === 'category' ? undefined : categories.find((c) => c.channel === parentId);
  const parentOverwrites = categoryOverwrites(parentCat?.channel);
  const followsParent = !!parentOverwrites && sameOverwrites(overwrites, parentOverwrites);
  // 카테고리를 고치면 지금 카테고리 권한과 같은(맞춰진) 채널도 함께 바꿉니다.
  const followers = existing && kind === 'category' ? syncedChildren(view, existing.channel, overwrites0 ?? []) : [];

  const editPerms = (next: PermissionOverwrite[]) => {
    setOverwrites(next);
    setPermsEdited(true);
  };
  const pickParent = (id: string | null) => {
    setParentId(id);
    // 만들 때 권한을 아직 손대지 않았으면 새로 고른 카테고리 권한에서 다시 시작합니다.
    if (!existing && !permsEdited) setOverwrites(categoryOverwrites(id) ?? []);
  };

  const pickSecurity = (v: SecurityLevel) => {
    setSecurity(v);
    if (v === 'volatile') {
      // 휘발성 기본값: 링크 미리보기 끔(외부 서버 접속 없음), 5분 수명
      setLinkPreviews(false);
      if (ttl === 'none') setTtl('seconds');
    }
    // 첨부 한도를 아직 손대지 않았으면 등급의 기본값으로 맞춥니다(휘발성은 모든 참가자 RAM에 올라가 10MiB).
    if (!limitTouched) setLimitText(v === 'volatile' ? VOLATILE_START_MIB : '');
  };

  const submit = async () => {
    if (!client || !name.trim() || blocked.length > 0 || tagError || limitError || capError) return;
    if (existing && s0) {
      // 바뀐 필드만 보냅니다(CLI·MCP로 바꾼 설정을 덮어쓰지 않게).
      const patch = channelUpdatePatch({ ...s0, overwrites: overwrites0 ?? [] }, form, existing);
      if (Object.keys(patch).length === 0) {
        toast('info', t('바뀐 설정이 없습니다'));
        return;
      }
      const synced = patch.overwrites ? followers : [];
      setBusy(true);
      const r = await call(client.call('channel.update', { space, channel: existing.channel, ...patch }), {
        ok: synced.length > 0 ? undefined : kind === 'category' ? t('카테고리 설정을 저장했습니다') : t('채널 설정을 저장했습니다'),
      });
      if (r !== undefined && patch.overwrites && synced.length > 0) {
        let done = 0;
        for (const c of synced) {
          const ok = await call(client.call('channel.update', { space, channel: c.channel, overwrites: patch.overwrites }));
          if (ok !== undefined) done++;
        }
        toast('success', t('카테고리 설정을 저장하고 따라오는 채널 {n}개의 권한도 맞췄습니다', { n: done }));
      }
      // 저장해도 창은 그대로 둡니다(이어서 다른 설정을 고칠 수 있게).
      setBusy(false);
    } else {
      setBusy(true);
      const r = await call(client.call('channel.create', channelCreateParams(space, kind, security, form, view.kind)));
      setBusy(false);
      if (r) {
        onClose();
        if (kind !== 'category') setTimeout(() => void selectChannel(space, r.channel), 150);
      }
    }
  };

  return (
    <Modal
      title={
        existing
          ? t('{name} 설정', { name: `${s0?.kind === 'category' ? '' : '#'}${s0?.name}` })
          : kind === 'category'
            ? t('카테고리 만들기')
            : t('채널 만들기')
      }
      onClose={onClose}
      size="lg"
      footer={
        <>
          {existing && (
            <button
              type="button"
              className="btn danger-outline left"
              onClick={() =>
                openModal({
                  kind: 'confirm',
                  title: t('채널 삭제'),
                  body: t('#{name} 채널을 지웁니다. 멤버 화면에서 사라지지만, 이미 받아 간 기록은 각자 기기에 남을 수 있습니다.', { name: s0?.name ?? '' }),
                  danger: true,
                  confirmLabel: t('삭제'),
                  onConfirm: async () => {
                    const r = await call(client!.call('channel.delete', { space, channel: existing.channel }));
                    // 지웠으면 지운 채널의 설정 창으로 돌아가지 않습니다(취소하면 설정 창이 그대로 남음).
                    if (r !== undefined) closeModal();
                  },
                })
              }
            >
              <Trash2 size={14} /> {t('채널 삭제')}
            </button>
          )}
          <button type="button" className="btn ghost" onClick={onClose}>
            {existing ? t('닫기') : t('취소')}
          </button>
          <button type="button" className="btn primary" disabled={busy || !name.trim() || blocked.length > 0 || !!tagError || !!limitError || !!capError} onClick={() => void submit()}>
            {existing ? t('저장') : t('만들기')}
          </button>
        </>
      }
    >
      <div className="seg modal-tabs" role="tablist">
        <button type="button" role="tab" aria-selected={tab === 'general'} className={tab === 'general' ? 'active' : ''} onClick={() => setTab('general')}>
          {t('일반')}
        </button>
        <button type="button" role="tab" aria-selected={tab === 'perms'} className={tab === 'perms' ? 'active' : ''} onClick={() => setTab('perms')}>
          {t('권한')}
        </button>
      </div>
      {blocked.length > 0 && (
        <div className="warn-box">
          <AlertTriangle size={16} />
          <span>
            {t('내게 없는 권한({list})의 설정이 바뀌어 저장할 수 없습니다. 그 권한은 원래대로 두세요.', {
              n: blocked.length,
              list: blocked.map((p) => PERMISSION_LABELS[p]).join(', '),
            })}
          </span>
        </div>
      )}
      {tab === 'general' && (
        <>
          {/* 카테고리는 사이드 메뉴의 '카테고리 만들기'로 따로 만듭니다. */}
          {!existing && kind !== 'category' && (
            <Field label={t('종류')}>
              <div className="kind-grid">
                {KINDS.map((k) => (
                  <button key={String(k.kind)} type="button" className={cx('kind-card', kind === k.kind && 'active')} onClick={() => setKind(k.kind)}>
                    {k.icon}
                    <span className="kc-label">{k.label}</span>
                    <span className="kc-desc">{k.desc}</span>
                  </button>
                ))}
              </div>
            </Field>
          )}
          <div className="grid2">
            <Field label={t('이름')}>
              <input
                className="input"
                value={name}
                maxLength={100}
                placeholder={kind === 'category' ? t('예: 대화') : t('예: 잡담')}
                onChange={(e) => setName(kind === 'category' ? e.target.value : e.target.value.replace(/\s+/g, '-').toLowerCase())}
              />
            </Field>
            {kind !== 'category' && categories.length > 0 && (
              <Field label={t('카테고리')}>
                <select className="select" value={parentId ?? ''} onChange={(e) => pickParent(e.target.value || null)}>
                  <option value="">{t('(없음)')}</option>
                  {categories.map((c) => (
                    <option key={c.channel} value={c.channel}>
                      {c.settings.name}
                    </option>
                  ))}
                </select>
              </Field>
            )}
          </div>
          {kind !== 'category' && (
            <Field label={t('주제')}>
              <input className="input" value={topic} maxLength={1024} onChange={(e) => setTopic(e.target.value)} placeholder={t('채널 위에 표시할 설명')} />
            </Field>
          )}

          {isText && (
            <Field label={t('보안 등급')} hint={existing ? t('보안 등급은 만든 뒤 바꿀 수 없습니다(이미 저장된 기록을 되돌릴 수 없으므로).') : undefined}>
              <div className="sec-grid">
                <button type="button" disabled={!!existing} className={cx('sec-card', security === 'normal' && 'active')} onClick={() => pickSecurity('normal')}>
                  <Hash size={20} />
                  <strong>{tc('보안 등급: 일반 채널', '일반')}</strong>
                  <span>{t('기록을 기기에 저장하고 시더가 나눠 보관합니다. 오프라인이어도 나중에 받을 수 있습니다.')}</span>
                </button>
                <button
                  type="button"
                  disabled={!!existing}
                  className={cx('sec-card volatile', security === 'volatile' && 'active')}
                  onClick={() => pickSecurity('volatile')}
                >
                  <Flame size={20} />
                  <strong>{t('휘발성')}</strong>
                  <span>{t('메모리에만 두고 디스크·시더에 남기지 않습니다. 수명이 지나면 모두에게서 지워집니다.')}</span>
                </button>
              </div>
            </Field>
          )}

          {lifetime && (
            <div className={cx('volatile-opts', !volatile && 'normal')}>
              <Field
                label={t('입장 방식')}
                hint={joinMode === 'join_leave' ? t('들어온 사람만 메시지를 받고, 나가면 더 받지 않습니다.') : t('크루 멤버 모두가 항상 받습니다.')}
              >
                <div className="seg">
                  <button type="button" className={joinMode === 'always' ? 'active' : ''} onClick={() => setJoinMode('always')}>
                    {t('항상 참여')}
                  </button>
                  <button type="button" className={joinMode === 'join_leave' ? 'active' : ''} onClick={() => setJoinMode('join_leave')}>
                    {t('입장·퇴장')}
                  </button>
                </div>
              </Field>
              <Field
                label={t('수명(TTL)')}
                hint={
                  ttl === 'none'
                    ? undefined
                    : volatile
                      ? t('각 기기가 받은 시점부터 잽니다. 만료되면 메모리에서 지웁니다.')
                      : t('각 기기가 받은 시점부터 잽니다. 만료되면 메시지별 키를 폐기해 디스크에서도 복구할 수 없게 합니다.')
                }
              >
                <div className="seg">
                  <button type="button" className={ttl === 'none' ? 'active' : ''} onClick={() => setTtl('none')}>
                    {t('없음')}
                  </button>
                  <button type="button" className={ttl === 'on_leave' ? 'active' : ''} onClick={() => setTtl('on_leave')}>
                    {t('퇴장 시 삭제')}
                  </button>
                  <button type="button" className={ttl === 'seconds' ? 'active' : ''} onClick={() => setTtl('seconds')}>
                    {t('시간')}
                  </button>
                </div>
                {ttl === 'seconds' && (
                  <div className="ttl-row">
                    {TTL_PRESETS.map((s) => (
                      <button key={s} type="button" className={cx('chip', ttlSecs === s && 'active')} onClick={() => setTtlSecs(s)}>
                        {formatDuration(s)}
                      </button>
                    ))}
                    <input
                      className="input small"
                      type="number"
                      min={TTL_MIN_SECS}
                      max={TTL_MAX_SECS}
                      value={ttlSecs}
                      onChange={(e) => setTtlSecs(clampTtl(Number(e.target.value) || TTL_MIN_SECS))}
                    />
                    <span className="muted small">{t('초')}</span>
                  </div>
                )}
              </Field>
            </div>
          )}

          {(isText || kind === 'forum') && (
            <div className="grid2">
              <Field label={t('새 멤버에게 과거 기록')} hint={volatile ? t('휘발성: 지금 참가자 메모리에 남은 것만') : undefined}>
                <div className="seg">
                  <button type="button" className={history === 'public' ? 'active' : ''} onClick={() => setHistory('public')}>
                    {t('공개')}
                  </button>
                  <button type="button" className={history === 'private' ? 'active' : ''} onClick={() => setHistory('private')}>
                    {t('비공개')}
                  </button>
                </div>
              </Field>
              <Field label={t('링크 미리보기')} hint={t('켜면 보내는 쪽이 미리보기를 만들어 함께 보냅니다(받는 쪽은 외부 서버에 접속하지 않음).')}>
                <Toggle checked={linkPreviews} onChange={setLinkPreviews} label={t('링크 미리보기')} />
              </Field>
            </div>
          )}

          {canEditTags && (
            // 입력·단추가 여럿이라 <label>(Field)이 아닌 묶음으로 둡니다.
            <section className="field" aria-label={t('포럼 태그')}>
              <span className="field-label">{t('태그')}</span>
              <ForumTagsEditor tags={forumTags} onChange={setForumTags} />
              <span className="field-hint">
                {tagError ? <span className="err">{tagError}</span> : t('글쓴이가 글에 붙일 태그(글마다 5개까지). 이모지도 이름에 바로 쓰세요.')}
              </span>
            </section>
          )}

          {canEditRules && (
            // 입력란 옆에 경고가 붙어 <label>(Field)이 아닌 묶음으로 둡니다.
            <section className="field" aria-label={t('첨부 용량 한도')}>
              <span className="field-label">{t('첨부 용량 한도')}</span>
              <div className="row gap-s attach-limit-row">
                <input
                  className={cx('input small', limitError && 'invalid')}
                  type="text"
                  inputMode="decimal"
                  placeholder={t('무제한')}
                  value={limitText}
                  aria-label={t('첨부 하나의 최대 크기(MiB)')}
                  aria-invalid={!!limitError}
                  onChange={(e) => {
                    setLimitTouched(true);
                    setLimitText(e.target.value);
                  }}
                />
                <span className="muted small">MiB</span>
                {limitBytes === 0 && !limitError && <span className="muted small">{t('첨부 금지')}</span>}
                {ramWarning && <span className="err ram-warning">{VOLATILE_RAM_WARNING}</span>}
              </div>
              <span className="field-hint">
                {limitError ? <span className="err">{limitError}</span> : ATTACH_LIMIT_HINT + (volatile ? ' ' + VOLATILE_LIMIT_HINT : '')}
              </span>
            </section>
          )}
          {canEditCap && (
            <Field label={t('참가 상한')} hint={capError ? <span className="err">{capError}</span> : VOLATILE_CAP_HINT}>
              <div className="row gap-s">
                <input
                  className={cx('input small', capError && 'invalid')}
                  type="text"
                  inputMode="numeric"
                  value={capText}
                  aria-label={t('상시 참여 최대 기기 수')}
                  aria-invalid={!!capError}
                  onChange={(e) => setCapText(e.target.value)}
                />
                <span className="muted small">{t('대')}</span>
              </div>
            </Field>
          )}
          {canEditRules && (
            <div className="grid2">
              <Field label={t('수정 금지')} hint={NO_EDIT_HINT}>
                <Toggle checked={noEdit} onChange={setNoEdit} label={t('수정 금지')} />
              </Field>
              <Field label={t('삭제 금지')} hint={NO_DELETE_HINT}>
                <Toggle checked={noDelete} onChange={setNoDelete} label={t('삭제 금지')} />
              </Field>
            </div>
          )}

          {p2pAllowed && !existing && (
            <Field
              label={<span className="row gap-s"><P2pIcon size={13} /> {t('P2P 전용')}</span>}
              hint={t('켜면 메시지·파일·통화가 Tor 대신 참가한 기기끼리 직접 오가, 참가자끼리 서로 IP 주소가 보입니다.')}
            >
              <Toggle checked={p2p} onChange={setP2p} label={t('P2P 전용')} />
            </Field>
          )}
          {p2pAllowed && existing && (
            <Field
              label={<span className="row gap-s"><P2pIcon size={13} /> {t('P2P 전용')}</span>}
              hint={
                existing.p2p
                  ? t('끄면 Tor로 되돌립니다. 이제부터 기록이 Tor로 모든 멤버에게 동기화되고, 이미 보인 IP 주소는 다시 숨길 수 없습니다.')
                  : t('켜면 참가자끼리 서로 IP 주소가 보이고, 멤버마다 경고를 받아들이고 참가해야 이 채널을 다시 쓸 수 있습니다.')
              }
            >
              <Toggle checked={existing.p2p} onChange={(v) => setChannelP2p(space, existing.channel, v)} label={t('P2P 전용')} />
            </Field>
          )}
          {anonAllowed && (
            <Field label={<span className="row gap-s"><AnonIcon size={13} /> {t('익명')}</span>} hint={ANON_HINT}>
              {existing ? (
                <Toggle checked={!!existing.anonymous} onChange={(v) => setChannelAnonymous(space, existing.channel, v)} label={t('익명')} />
              ) : (
                <Toggle checked={anonymous} onChange={setAnonymous} label={t('익명')} />
              )}
            </Field>
          )}
        </>
      )}
      {tab === 'perms' && (
        <>
          {parentCat && parentOverwrites && (
            <div className={followsParent ? 'ok-box' : 'note'}>
              {followsParent ? <Link2 size={16} /> : <Unlink size={16} />}
              <span className="grow">
                {followsParent
                  ? t('「{name}」 카테고리 권한과 같습니다. 카테고리 권한을 바꾸면 이 채널도 함께 바뀝니다.', { name: parentCat.settings.name })
                  : t('「{name}」 카테고리 권한과 다릅니다.', { name: parentCat.settings.name })}
              </span>
              {!followsParent && (
                <button
                  type="button"
                  className="btn small"
                  onClick={() => {
                    setOverwrites(parentOverwrites);
                    setPermsEdited(false);
                  }}
                >
                  {t('카테고리 권한으로 맞추기')}
                </button>
              )}
            </div>
          )}
          {kind === 'category' && (
            <div className="note">
              <Link2 size={16} />
              <span>
                {t('카테고리 권한은 안의 채널에 저절로 적용되지 않습니다.')}{' '}
                {existing
                  ? followers.length > 0
                    ? t('지금 이 카테고리 권한과 같은 채널 {n}개는 저장할 때 함께 맞춥니다.', { n: followers.length })
                    : t('지금 이 카테고리 권한과 같은 채널이 없어 카테고리만 바뀝니다.')
                  : t('이 카테고리 안에 새로 만드는 채널은 이 권한에서 시작합니다.')}
              </span>
            </div>
          )}
          <ChannelPermsEditor view={view} kind={kind} overwrites={overwrites} onChange={editPerms} />
        </>
      )}
    </Modal>
  );
}
