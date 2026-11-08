// 크루 설정: 개요(이 크루 신뢰 스위치, 크루 STUN 서버와 그 서버 쓰기 스위치, 엄격한 키 회전 포함), 역할·권한(순위·표시·템플릿), 멤버, 초대, 커스텀 이모지, 시더 커버리지, 모더레이션(한계 안내·신고·밴).
// 관리자를 볼 수 없는 사람(`admins_hidden`)에게는 소유자 표시와 확정 기기 후보(소유자·관리자의 기기)를 보이지 않습니다.

import {
  AlertTriangle,
  ArrowDown,
  ArrowUp,
  Ban,
  Bot,
  Copy,
  Crown,
  Flame,
  ImagePlus,
  Plus,
  Shield,
  SmilePlus,
  Trash2,
  UserX,
} from 'lucide-react';
import { useCallback, useEffect, useMemo, useState } from 'react';
import type { PickedFile } from '../api/client';
import type { ChannelCoverage, InviteView, Role, RoleId, SeederPolicy } from '../api/types';
import { EVERYONE_ROLE } from '../api/types';
import type { EmojiLimits } from '../api/types';
import { Avatar, ChannelIcon, CustomEmoji, EmojiText, NoAddressWarning, Popover, SpaceIcon, Spinner, Toggle, copyText } from '../components/common';
import { EmojiPicker } from '../components/EmojiPicker';
import { t, tc, tn } from '../i18n';
import { MAX_EMOJIS, emojiPlain, hasEmoji, parseEmojiLimits, validEmojiName } from '../lib/emoji';
import { formatBytes, formatFull } from '../lib/format';
import { PERMISSION_LABELS, byLevelDesc, canEditRole, canGrant, cx, displayName, hasPerm, hexToInt, isAdminRole, linkLacksAddress, outranks, permissionBits, permissionNames, rankOf, roleColor, roleLevel } from '../lib/misc';
import { PERM_GROUPS, applyTemplate, roleDisplayOf, roleDisplayPatch } from '../lib/perms';
import { useStore, type CrewTab } from '../store/store';
import { Field, Modal } from './Modal';
import { CommitterSection } from './Committer';
import { ModerationPanel } from './Moderation';
import { ArchiveNodes } from './ArchiveNodes';
import { PermTemplateBar } from './PermTemplates';
import { CrewTrustField } from './Trust';
import { CrewServersSection, CrewServersUseField } from './CrewServers';

const TABS: Array<[CrewTab, string]> = [
  ['overview', t('개요')],
  ['roles', t('역할·권한')],
  ['members', t('멤버')],
  ['invites', t('초대')],
  ['emoji', t('이모지')],
  ['seeders', t('시더 커버리지')],
  ['moderation', t('모더레이션')],
];

export function CrewSettingsModal({ space, tab, onClose }: { space: string; tab?: CrewTab; onClose: () => void }) {
  const view = useStore((s) => s.views[space]);
  const [current, setCurrent] = useState<CrewTab>(tab ?? 'overview');
  const reports = useStore((s) => s.reportsNew[space] ?? 0);
  if (!view) return null;
  return (
    <Modal onClose={onClose} size="full" className="settings-modal" title={t('{name} 설정', { name: view.settings.name })}>
      <div className="settings">
        <nav className="settings-nav">
          <div className="sn-title">{view.settings.name}</div>
          {TABS.map(([id, label]) => (
            <button key={id} type="button" className={cx('sn-item', current === id && 'active')} onClick={() => setCurrent(id)}>
              {label}
              {id === 'moderation' && reports > 0 && current !== 'moderation' && <span className="ch-badge">{reports}</span>}
            </button>
          ))}
        </nav>
        <section className="settings-body">
          {current === 'overview' && <Overview space={space} />}
          {current === 'roles' && <Roles space={space} />}
          {current === 'members' && <Members space={space} />}
          {current === 'invites' && <Invites space={space} />}
          {current === 'emoji' && <Emojis space={space} />}
          {current === 'seeders' && <Seeders space={space} />}
          {current === 'moderation' && <ModerationPanel space={space} />}
        </section>
      </div>
    </Modal>
  );
}

function Overview({ space }: { space: string }) {
  const view = useStore((s) => s.views[space])!;
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const me = useStore((s) => s.me);
  const openModal = useStore((s) => s.openModal);
  const [name, setName] = useState(view.settings.name);
  const [desc, setDesc] = useState(view.settings.description);
  const myMember = view.members.find((m) => m.user === me);
  const [nick, setNick] = useState(myMember?.nickname ?? '');
  // 고른 새 아이콘, 지우기(빈 경로), 그대로(null)
  const [icon, setIcon] = useState<PickedFile | 'clear' | null>(null);
  const [owner, setOwner] = useState('');
  const canManage = view.my_permissions.includes('manage_space') || view.my_permissions.includes('administrator');
  const dirty = name !== view.settings.name || desc !== view.settings.description || icon;
  return (
    <div className="stack">
      <h3>{t('개요')}</h3>
      <div className="row gap align-start">
        <button
          type="button"
          className="icon-upload"
          disabled={!canManage}
          title={t('크루 아이콘')}
          onClick={async () => {
            const f = await client?.transport.pickFiles({ multiple: false, images: true });
            if (f?.[0]) setIcon(f[0]);
          }}
        >
          {icon && icon !== 'clear' ? (
            <span className="small">{icon.name}</span>
          ) : view.settings.icon && icon !== 'clear' ? (
            <SpaceIcon space={space} icon={view.settings.icon} name={view.settings.name} size={80} />
          ) : (
            <ImagePlus size={26} />
          )}
        </button>
        {canManage && (view.settings.icon || (icon && icon !== 'clear')) && icon !== 'clear' && (
          <button type="button" className="btn ghost small" onClick={() => setIcon(view.settings.icon ? 'clear' : null)}>
            {t('아이콘 지우기')}
          </button>
        )}
        <div className="grow stack">
          <Field label={t('크루 이름')}>
            <input className="input" value={name} maxLength={100} disabled={!canManage} onChange={(e) => setName(e.target.value)} />
          </Field>
          <Field label={t('설명')}>
            <textarea className="input" rows={3} value={desc} maxLength={1000} disabled={!canManage} onChange={(e) => setDesc(e.target.value)} />
          </Field>
        </div>
      </div>
      {dirty && canManage && (
        <div className="save-bar">
          <span>{t('저장하지 않은 변경이 있습니다')}</span>
          <button
            type="button"
            className="btn primary small"
            onClick={async () => {
              if (!client) return;
              const icon_path = icon === null ? undefined : icon === 'clear' ? '' : icon.path;
              const r = await call(client.call('crew.update', { space, name: name.trim(), description: desc, icon_path }), { ok: t('저장했습니다') });
              if (r !== undefined) setIcon(null);
            }}
          >
            {t('저장')}
          </button>
        </div>
      )}
      <Field label={t('이 크루에서 쓸 내 별명')} hint={hasEmoji(nick) ? t('별명에 이모지를 넣을 수 없습니다') : t('비우면 프로필 이름을 씁니다.')}>
        <div className="row gap">
          <input className="input grow" value={nick} maxLength={64} onChange={(e) => setNick(e.target.value)} />
          <button
            type="button"
            className="btn small"
            disabled={nick === (myMember?.nickname ?? '') || hasEmoji(nick)}
            onClick={() => client && void call(client.call('profile.set_crew', { space, nickname: nick.trim() || undefined }), { ok: t('별명을 바꿨습니다') })}
          >
            {t('적용')}
          </button>
        </div>
      </Field>
      <CrewTrustField space={space} />
      <CrewServersUseField space={space} />
      <CrewServersSection space={space} />
      {typeof view.settings.strict_rotation === 'boolean' && (
        <Field
          label={t('엄격한 키 회전')}
          hint={t(
            '켜면 나간·내보낸 기기를 뺀 키 갱신이 올 때까지 메시지를 보내지 않고 기다립니다(확정 기기가 오프라인이면 보내기가 멈춤). 끄면 잠시 기다린 뒤 옛 키로 보내고 ‘키 회전 대기 중’을 표시합니다.',
          )}
        >
          <Toggle
            checked={view.settings.strict_rotation}
            disabled={!canManage}
            label={t('엄격한 키 회전')}
            onChange={(enabled) =>
              client && void call(client.call('crew.update', { space, strict_rotation: enabled }), { ok: enabled ? t('엄격한 키 회전을 켰습니다') : t('엄격한 키 회전을 껐습니다') })
            }
          />
        </Field>
      )}
      <div className="kv">
        <span>{t('다른 공간 이모지')}</span>
        <span>
          {view.settings.allow_external_emoji ? t('허용(역할 권한 ‘다른 공간 이모지 쓰기’가 있는 멤버만)') : t('꺼짐(이 크루에서는 아무도 쓸 수 없음)')}
        </span>
        <span>{t('멤버')}</span>
        <span>{t('{n}명', { n: view.members.length })}</span>
        <span>{t('확정 노드')}</span>
        <span className="mono small">{view.committer.slice(0, 16)}…</span>
        {!view.admins_hidden && (
          <>
            <span>{t('소유자')}</span>
            <span>{displayName(view.members.find((m) => m.user === view.owner))}</span>
          </>
        )}
      </div>
      {!view.admins_hidden && <CommitterSection space={space} />}
      {view.owner === me && (
        <Field label={<span className="row gap-s"><Crown size={13} /> {t('소유권 넘기기')}</span>} hint={t('넘기면 되돌릴 수 없습니다. 새 소유자가 크루의 최종 권한을 갖습니다.')}>
          <div className="row gap">
            <select className="select grow" value={owner} onChange={(e) => setOwner(e.target.value)}>
              <option value="">{t('멤버 고르기')}</option>
              {view.members
                .filter((m) => m.user !== me && !m.automation)
                .map((m) => (
                  <option key={m.user} value={m.user}>
                    {displayName(m)}
                  </option>
                ))}
            </select>
            <button
              type="button"
              className="btn danger-outline small"
              disabled={!owner}
              onClick={() =>
                openModal({
                  kind: 'confirm',
                  title: t('소유권 넘기기'),
                  body: t('{name} 님에게 소유권을 넘깁니다.', { name: displayName(view.members.find((m) => m.user === owner)) }),
                  danger: true,
                  confirmLabel: t('넘기기'),
                  onConfirm: async () => {
                    if (client) await call(client.call('crew.transfer_ownership', { space, user: owner }), { ok: t('소유권을 넘겼습니다') });
                  },
                })
              }
            >
              {t('넘기기')}
            </button>
          </div>
        </Field>
      )}
    </div>
  );
}

function Roles({ space }: { space: string }) {
  const view = useStore((s) => s.views[space])!;
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const openModal = useStore((s) => s.openModal);
  const roles = useMemo(() => view.roles.slice().sort(byLevelDesc), [view.roles]);
  const [sel, setSel] = useState<string>(roles.find((r) => r.id !== EVERYONE_ROLE)?.id ?? EVERYONE_ROLE);
  const role = roles.find((r) => r.id === sel) ?? roles[0];
  const me = useStore((s) => s.me);
  const canManage = hasPerm(view, 'manage_roles');
  const myRank = rankOf(view, me);
  const canSwap = (a: Role, b: Role) => isAdminRole(a) === isAdminRole(b) && canEditRole(view, me, b);

  const move = async (r: Role, dir: -1 | 1) => {
    if (!client) return;
    // 이웃 역할과 순위를 맞바꿉니다(@everyone은 항상 맨 아래).
    const list = roles.filter((x) => x.id !== EVERYONE_ROLE);
    const i = list.findIndex((x) => x.id === r.id);
    const other = list[i + dir];
    // 내 순위 이상인 역할과는 맞바꿀 수 없고, 관리자 역할과 일반 역할은 순위를 바꿔도 위아래가 그대로라 맞바꾸지 않습니다.
    if (!other || !canSwap(r, other)) return;
    const a = r.rank;
    const b = other.rank === a ? a + dir * -1 : other.rank;
    await call(client.call('role.update', { space, role: r.id, rank: b }));
    await call(client.call('role.update', { space, role: other.id, rank: a }));
  };

  return (
    <div className="roles-layout">
      <div className="role-list">
        <div className="row between">
          <h3>{t('역할')}</h3>
          {canManage && (
            <button
              type="button"
              className="btn small primary"
              onClick={async () => {
                if (!client) return;
                // 새 역할(일반 역할)은 내가 고칠 수 있는 일반 역할 중 맨 위 바로 위(내 순위보다는 낮게)에 둡니다. 관리자는
                // 일반 역할 전부보다 위라 일반 역할 맨 위에 놓입니다.
                const below = roles.filter((r) => r.id !== EVERYONE_ROLE && !isAdminRole(r) && r.rank < myRank).map((r) => r.rank);
                const rank = Math.max(1, Math.min(myRank - 1, Math.max(0, ...below) + 1));
                const base = ['view_channel', 'send_messages', 'read_history', 'add_reactions'].filter((p) => canGrant(view, p));
                const r = await call(client.call('role.create', { space, name: t('새 역할'), color: 0x99aab5, permissions: base, rank }));
                if (r) setSel(r.role);
              }}
            >
              <Plus size={14} /> {t('만들기')}
            </button>
          )}
        </div>
        <p className="muted small">
          {t('위에 있을수록 순위가 높습니다. 관리자 권한이 있는 역할은 늘 일반 역할보다 위에 있습니다. 자기 최고 역할과 같거나 높은 역할은 바꾸거나 옮길 수 없습니다.')}
        </p>
        {roles.map((r, i) => {
          const editable = canEditRole(view, me, r);
          return (
          <div key={r.id} className={cx('role-item', sel === r.id && 'active')}>
            <button type="button" className="role-pick" onClick={() => setSel(r.id)}>
              <span className="role-dot" style={{ background: roleColor(r.color) ?? 'var(--text-muted)' }} />
              <span className="grow"><EmojiText text={r.name} /></span>
              {r.automation && <Bot size={14} className="muted" />}
              <span className="muted small">{view.members.filter((m) => m.roles.includes(r.id)).length || ''}</span>
            </button>
            {editable && r.id !== EVERYONE_ROLE && (
              <span className="role-move">
                <button
                  type="button"
                  className="icon-btn small"
                  title={t('위로')}
                  disabled={i === 0 || !roles[i - 1] || !canSwap(r, roles[i - 1])}
                  onClick={() => void move(r, -1)}
                >
                  <ArrowUp size={13} />
                </button>
                <button type="button" className="icon-btn small" title={t('아래로')} disabled={!roles[i + 1] || roles[i + 1].id === EVERYONE_ROLE || !canSwap(r, roles[i + 1])} onClick={() => void move(r, 1)}>
                  <ArrowDown size={13} />
                </button>
              </span>
            )}
          </div>
          );
        })}
      </div>
      {role && (
        <RoleEditor
          key={role.id + role.permissions + role.name + role.color + JSON.stringify(view.role_display?.[role.id] ?? null)}
          space={space}
          role={role}
          canManage={canEditRole(view, me, role)}
          onDelete={() =>
            openModal({
              kind: 'confirm',
              title: t('역할 삭제'),
              body: t('「{name}」 역할을 지웁니다. 이 역할을 가진 멤버는 역할만 잃습니다.', { name: emojiPlain(role.name) }),
              danger: true,
              confirmLabel: t('삭제'),
              onConfirm: async () => {
                if (client) await call(client.call('role.delete', { space, role: role.id }));
                setSel(EVERYONE_ROLE);
              },
            })
          }
        />
      )}
    </div>
  );
}

function RoleEditor({ space, role, canManage, onDelete }: { space: string; role: Role; canManage: boolean; onDelete: () => void }) {
  const view = useStore((s) => s.views[space])!;
  const me = useStore((s) => s.me);
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const [name, setName] = useState(role.name);
  const [picker, setPicker] = useState<DOMRect | null>(null);
  const [color, setColor] = useState(roleColor(role.color) ?? '#99aab5');
  const [perms, setPerms] = useState<string[]>(permissionNames(role.permissions));
  const display0 = roleDisplayOf(view, role.id);
  const [visibleTo, setVisibleTo] = useState<RoleId[] | null>(display0.visible_to);
  const [seeAdmins, setSeeAdmins] = useState(display0.see_admins);
  const everyone = role.id === EVERYONE_ROLE;
  const displayPatch = roleDisplayPatch(display0, { visible_to: visibleTo, see_admins: seeAdmins }, everyone);
  // @everyone은 이름·색을 바꾸지 않고, 색이 없는 역할은 화면 기본색과 비교합니다(열자마자 변경으로 보이지 않게).
  const dirty =
    (!everyone && (name !== role.name || color !== (roleColor(role.color) ?? '#99aab5'))) ||
    permissionBits(perms) !== permissionBits(permissionNames(role.permissions)) ||
    Object.keys(displayPatch).length > 0;
  // 표시 설정은 역할을 관리할 수 있는 사람에게만 옵니다(없으면 기본값이라 보여 줄 것이 없음).
  const showDisplay = hasPerm(view, 'manage_roles');
  // 관리자 권한을 켜면 관리자 역할 사이의 순위로 올라가므로, 그래도 내 순위보다 낮을 때만 켤 수 있습니다(데몬 below_me).
  const adminFits = rankOf(view, me) > roleLevel({ ...role, permissions: (role.permissions | 0x8000_0000) >>> 0 });
  const mayToggle = (p: string) => canGrant(view, p) && (p !== 'administrator' || perms.includes(p) || adminFits);
  const others = view.roles.filter((r) => r.id !== EVERYONE_ROLE && r.id !== role.id).sort(byLevelDesc);
  return (
    <div className="role-editor">
      <div className="row between">
        <h3>{everyone ? t('@everyone(모든 멤버)') : tn('역할 편집 — {name}', { name: <EmojiText text={role.name} /> })}</h3>
        {!everyone && canManage && (
          <button type="button" className="btn small danger-outline" onClick={onDelete}>
            <Trash2 size={13} /> {t('삭제')}
          </button>
        )}
      </div>
      {!everyone && <p className="muted small">{t('역할 권한은 @everyone 권한에 더해집니다. 채널별 덮어쓰기는 채널 설정에서 바꿉니다.')}</p>}
      {!canManage && hasPerm(view, 'manage_roles') && <p className="muted small">{t('내 순위와 같거나 높은 역할이라 바꿀 수 없습니다.')}</p>}
      {canManage && <p className="muted small">{t('자기에게 없는 권한은 주거나 뺄 수 없습니다.')}</p>}
      {role.automation && (
        <div className="note">
          <Bot size={16} /> {t('AI·자동화용 역할입니다. 파괴적 작업은 사람이 확인해야 실행됩니다.')}
        </div>
      )}
      {!everyone && (
        <div className="grid2">
          <Field label={t('이름')}>
            <div className="row gap-s">
              <input className="input grow" value={name} maxLength={100} disabled={!canManage} onChange={(e) => setName(e.target.value)} />
              <button type="button" className="icon-btn" title={t('이모지 넣기')} disabled={!canManage} onClick={(e) => setPicker(e.currentTarget.getBoundingClientRect())}>
                <SmilePlus size={18} />
              </button>
            </div>
            {name !== emojiPlain(name) && (
              <span className="role-chip small role-name-preview">
                <EmojiText text={name} />
              </span>
            )}
            {picker && (
              <Popover anchor={picker} onClose={() => setPicker(null)}>
                <EmojiPicker
                  space={space}
                  onPick={(emoji) => {
                    setPicker(null);
                    // 100자를 넘으면 넣지 않습니다(잘라 넣으면 표기가 반쯤 깨진 채 저장됨).
                    setName((n) => (Array.from(n + emoji).length <= 100 ? n + emoji : n));
                  }}
                />
              </Popover>
            )}
          </Field>
          <Field label={t('색')}>
            <div className="row gap">
              <input type="color" className="color" value={color} disabled={!canManage} onChange={(e) => setColor(e.target.value)} />
              <span className="mono small">{color}</span>
            </div>
          </Field>
        </div>
      )}
      {showDisplay && (
        <div className="perm-group">
          <div className="perm-title">{t('표시')}</div>
          {!everyone && (
            <div className="perm-block">
              <div className="row between">
                <span>{t('이 역할을 볼 수 있는 사람')}</span>
                <div className="seg">
                  <button type="button" className={visibleTo === null ? 'active' : ''} disabled={!canManage} onClick={() => setVisibleTo(null)}>
                    {t('모든 멤버')}
                  </button>
                  <button type="button" className={visibleTo !== null ? 'active' : ''} disabled={!canManage} onClick={() => setVisibleTo(visibleTo ?? [])}>
                    {t('고른 역할만')}
                  </button>
                </div>
              </div>
              {visibleTo !== null && (
                <div className="chips">
                  {others.length === 0 && <span className="muted small">{t('고를 다른 역할이 없습니다.')}</span>}
                  {others.map((r) => (
                    <button
                      key={r.id}
                      type="button"
                      className={cx('chip', visibleTo.includes(r.id) && 'active')}
                      disabled={!canManage}
                      onClick={() => setVisibleTo(visibleTo.includes(r.id) ? visibleTo.filter((x) => x !== r.id) : [...visibleTo, r.id])}
                    >
                      <span className="role-dot" style={{ background: roleColor(r.color) ?? 'var(--text-muted)' }} />
                      <EmojiText text={r.name} />
                    </button>
                  ))}
                </div>
              )}
              <span className="field-hint">
                {t('이 역할을 가진 사람과 역할을 관리할 수 있는 사람은 늘 봅니다. 화면에서만 숨깁니다: 작성자 권한을 확인해야 해서 멤버 기기에는 누가 어떤 역할인지 남아 있습니다.')}
              </span>
            </div>
          )}
          <div className="perm-row">
            <span>{t('관리자 보기')}</span>
            <Toggle checked={seeAdmins} disabled={!canManage} label={t('관리자 보기')} onChange={setSeeAdmins} />
          </div>
          <span className="field-hint">
            {everyone
              ? t('켜 두면 모든 멤버가 관리자(소유자와 관리자 권한 역할)가 누구인지 봅니다. 끄면 관리자 보기를 켠 역할을 가진 멤버만 봅니다.')
              : t('켜면 이 역할을 가진 멤버가 관리자(소유자와 관리자 권한 역할)가 누구인지 봅니다. @everyone의 관리자 보기가 꺼져 있을 때만 차이가 납니다.')}{' '}
            {t('화면에서만 숨깁니다.')}
          </span>
        </div>
      )}
      {canManage && (
        <PermTemplateBar
          scope={t('역할')}
          saveLabel={t('지금 권한을 템플릿으로 저장')}
          hint={t('불러오면 아래 권한 스위치만 바뀌고, 저장해야 역할에 적용됩니다. 내 템플릿은 이 기기의 앱 설정에 남습니다.')}
          current={{ permissions: perms }}
          onLoad={(tpl) => {
            const r = applyTemplate(perms, tpl.permissions, mayToggle);
            setPerms(r.permissions);
            return r.skipped;
          }}
        />
      )}
      {PERM_GROUPS.map(([group, list]) => (
        <div key={group} className="perm-group">
          <div className="perm-title">{group}</div>
          {list.map((p) => (
            <div key={p} className="perm-row">
              <span>
                {PERMISSION_LABELS[p]}
                {p === 'administrator' && <span className="tag warn">{t('위험')}</span>}
              </span>
              <Toggle
                checked={perms.includes(p)}
                disabled={!canManage || !mayToggle(p)}
                label={PERMISSION_LABELS[p]}
                onChange={(v) => setPerms(v ? [...perms, p] : perms.filter((x) => x !== p))}
              />
            </div>
          ))}
        </div>
      ))}
      {dirty && canManage && (
        <div className="save-bar sticky">
          <span>{t('저장하지 않은 변경이 있습니다')}</span>
          <button
            type="button"
            className="btn ghost small"
            onClick={() => {
              setName(role.name);
              setColor(roleColor(role.color) ?? '#99aab5');
              setPerms(permissionNames(role.permissions));
              setVisibleTo(display0.visible_to);
              setSeeAdmins(display0.see_admins);
            }}
          >
            {t('되돌리기')}
          </button>
          <button
            type="button"
            className="btn primary small"
            onClick={() =>
              client &&
              void call(
                client.call('role.update', {
                  space,
                  role: role.id,
                  name: everyone ? undefined : name.trim(),
                  color: everyone ? undefined : hexToInt(color),
                  permissions: perms,
                  ...displayPatch,
                }),
                { ok: t('역할을 저장했습니다') },
              )
            }
          >
            {t('저장')}
          </button>
        </div>
      )}
    </div>
  );
}

function Members({ space }: { space: string }) {
  const view = useStore((s) => s.views[space])!;
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const me = useStore((s) => s.me);
  const openModal = useStore((s) => s.openModal);
  const [q, setQ] = useState('');
  const [edit, setEdit] = useState<{ user: string; rect: DOMRect } | null>(null);
  const canRoles = hasPerm(view, 'manage_roles');
  const canKick = hasPerm(view, 'kick');
  const canBan = hasPerm(view, 'ban');
  const myRank = rankOf(view, me);
  const roles = view.roles.filter((r) => r.id !== EVERYONE_ROLE).sort(byLevelDesc);
  const list = view.members.filter((m) => displayName(m).toLowerCase().includes(q.toLowerCase()));
  const editing = edit ? view.members.find((m) => m.user === edit.user) : undefined;
  return (
    <div className="stack">
      <div className="row between">
        <h3>{t('멤버 {n}명', { n: view.members.length })}</h3>
        <input className="input small" placeholder={t('이름으로 찾기')} value={q} onChange={(e) => setQ(e.target.value)} />
      </div>
      <div className="table">
        {list.map((m) => (
          <div key={m.user} className="tr">
            <Avatar id={m.user} name={displayName(m)} size={32} online={m.online} />
            <div className="grow">
              <div className="row gap-s">
                <strong>{displayName(m)}</strong>
                {m.automation && <span className="tag bot">AI</span>}
                {m.user === view.owner && !view.admins_hidden && <Crown size={13} className="gold" />}
                {m.blocked && <span className="tag danger">{t('차단함')}</span>}
              </div>
              <div className="chips">
                {roles
                  .filter((r) => m.roles.includes(r.id))
                  .map((r) => (
                    <span key={r.id} className="role-chip small">
                      <span className="role-dot" style={{ background: roleColor(r.color) ?? 'var(--text-muted)' }} />
                      <EmojiText text={r.name} />
                    </span>
                  ))}
                {canRoles && (m.user === me || outranks(view, me, m.user)) && (
                  <button type="button" className="role-chip small add" onClick={(e) => setEdit({ user: m.user, rect: e.currentTarget.getBoundingClientRect() })}>
                    <Plus size={11} /> {t('역할')}
                  </button>
                )}
              </div>
            </div>
            {canBan && outranks(view, me, m.user) && (
              <button type="button" className="icon-btn danger" title={t('밴(추방하고 다시 못 들어오게)')} onClick={() => openModal({ kind: 'ban', space, user: m.user })}>
                <Ban size={17} />
              </button>
            )}
            {canKick && outranks(view, me, m.user) && (
              <button
                type="button"
                className="icon-btn danger"
                title={t('추방')}
                onClick={() =>
                  openModal({
                    kind: 'confirm',
                    title: t('{name} 추방', { name: displayName(m) }),
                    body: t('추방하면 크루 키를 바꿔 앞으로의 메시지를 받지 못하게 합니다. 이미 받은 기록은 상대 기기에 남습니다.'),
                    danger: true,
                    confirmLabel: t('추방'),
                    onConfirm: async () => {
                      if (client) await call(client.call('member.kick', { space, user: m.user }), { ok: t('추방했습니다') });
                    },
                  })
                }
              >
                <UserX size={17} />
              </button>
            )}
          </div>
        ))}
      </div>
      {edit && editing && (
        <Popover anchor={edit.rect} onClose={() => setEdit(null)} placement="bottom-start" className="menu">
          {roles.map((r) => {
            const has = editing.roles.includes(r.id);
            return (
              <label key={r.id} className={cx('menu-item check', roleLevel(r) >= myRank && 'disabled')} title={roleLevel(r) >= myRank ? t('내 순위와 같거나 높은 역할') : undefined}>
                <input
                  type="checkbox"
                  checked={has}
                  disabled={roleLevel(r) >= myRank}
                  onChange={() => {
                    if (!client) return;
                    const next = has ? editing.roles.filter((x) => x !== r.id) : [...editing.roles, r.id];
                    void call(client.call('member.set_roles', { space, user: editing.user, roles: next }));
                  }}
                />
                <span className="role-dot" style={{ background: roleColor(r.color) ?? 'var(--text-muted)' }} />
                <EmojiText text={r.name} />
              </label>
            );
          })}
        </Popover>
      )}
    </div>
  );
}

const INVITE_EXPIRY: Array<[string, number | undefined]> = [
  [t('{n}분', { n: 30 }), 1800],
  [t('{n}시간', { n: 1 }), 3600],
  [t('{n}일', { n: 1 }), 86_400],
  [t('{n}일', { n: 7 }), 7 * 86_400],
  [t('만료 없음'), undefined],
];
const INVITE_USES: Array<[string, number | undefined]> = [
  [t('{n}회', { n: 1 }), 1],
  [t('{n}회', { n: 5 }), 5],
  [t('{n}회', { n: 10 }), 10],
  [t('{n}회', { n: 25 }), 25],
  [t('무제한'), undefined],
];

function Invites({ space }: { space: string }) {
  const view = useStore((s) => s.views[space])!;
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const toast = useStore((s) => s.toast);
  const [list, setList] = useState<InviteView[] | null>(null);
  const [exp, setExp] = useState(2);
  const [uses, setUses] = useState(2);
  const [grant, setGrant] = useState<string[]>([]);
  const [url, setUrl] = useState<string | null>(null);
  const [noAddress, setNoAddress] = useState(false);
  const roles = view.roles.filter((r) => r.id !== EVERYONE_ROLE).sort(byLevelDesc);
  const load = useCallback(() => client?.call('invite.list', { space }).then(setList).catch(() => setList([])), [client, space]);
  useEffect(() => {
    void load();
  }, [load]);
  return (
    <div className="stack">
      <h3>{t('초대')}</h3>
      <div className="card">
        <div className="grid2">
          <Field label={tc('초대 만료 기간 칸', '만료')}>
            <div className="seg">
              {INVITE_EXPIRY.map(([l], i) => (
                <button key={l} type="button" className={exp === i ? 'active' : ''} onClick={() => setExp(i)}>
                  {l}
                </button>
              ))}
            </div>
          </Field>
          <Field label={t('사용 횟수')}>
            <div className="seg">
              {INVITE_USES.map(([l], i) => (
                <button key={l} type="button" className={uses === i ? 'active' : ''} onClick={() => setUses(i)}>
                  {l}
                </button>
              ))}
            </div>
          </Field>
        </div>
        <Field label={t('들어오면 줄 역할')}>
          <div className="chips">
            {roles.map((r) => (
              <button
                key={r.id}
                type="button"
                className={cx('chip', grant.includes(r.id) && 'active')}
                onClick={() => setGrant(grant.includes(r.id) ? grant.filter((x) => x !== r.id) : [...grant, r.id])}
              >
                <EmojiText text={r.name} />
              </button>
            ))}
          </div>
        </Field>
        <button
          type="button"
          className="btn primary"
          onClick={async () => {
            if (!client) return;
            const r = await call(client.call('invite.create', { space, expires_in_secs: INVITE_EXPIRY[exp][1], max_uses: INVITE_USES[uses][1], roles: grant }));
            if (r) {
              setUrl(r.url);
              setNoAddress(linkLacksAddress(await client.call('net.status', {}).catch(() => null)));
              void load();
            }
          }}
        >
          {t('초대 링크 만들기')}
        </button>
        {url && (
          <div className="link-box">
            <code className="mono">{url}</code>
            <button type="button" className="btn small" onClick={async () => (await copyText(url)) && toast('success', t('복사했습니다'))}>
              <Copy size={14} /> {t('복사')}
            </button>
          </div>
        )}
        {url && noAddress && <NoAddressWarning />}
      </div>
      <div className="table">
        {list === null && <Spinner />}
        {list?.length === 0 && <p className="muted">{t('활성 초대가 없습니다.')}</p>}
        {list?.map((inv) => (
          <div key={inv.token_hash} className="tr">
            <code className="mono small">{inv.token_hash.slice(0, 12)}…</code>
            <span className="grow small">
              {t('{name} 만듦', { name: displayName(view.members.find((m) => m.user === inv.created_by)) })} ·{' '}
              {inv.max_uses ? t('사용 {n}/{max}', { n: inv.uses, max: inv.max_uses }) : t('사용 {n}', { n: inv.uses })} ·{' '}
              {inv.expires_at ? t('{when} 만료', { when: formatFull(inv.expires_at) }) : t('만료 없음')}
              {inv.roles.length > 0 &&
                ' · ' + t('역할 {list}', { n: inv.roles.length, list: inv.roles.map((r) => emojiPlain(view.roles.find((x) => x.id === r)?.name ?? '?')).join(', ') })}
            </span>
            <button
              type="button"
              className="icon-btn danger"
              title={t('폐기')}
              onClick={async () => {
                if (!client) return;
                await call(client.call('invite.revoke', { space, token_hash: inv.token_hash }), { ok: t('초대를 폐기했습니다') });
                void load();
              }}
            >
              <Trash2 size={16} />
            </button>
          </div>
        ))}
      </div>
    </div>
  );
}

function Emojis({ space }: { space: string }) {
  const view = useStore((s) => s.views[space])!;
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const refreshSpace = useStore((s) => s.refreshSpace);
  const [file, setFile] = useState<PickedFile | null>(null);
  const [name, setName] = useState('');
  const canManage = view.my_permissions.includes('manage_emoji') || view.my_permissions.includes('administrator');
  const valid = validEmojiName(name);
  const limits = view.emoji_limits;
  const maxCount = Math.min(MAX_EMOJIS, limits?.max_count ?? MAX_EMOJIS);
  const members = new Map(view.members.map((m) => [m.user, m]));
  return (
    <div className="stack">
      <h3>
        {t('커스텀 이모지 {n}/{max}개', { n: view.emojis.length, max: maxCount })}
      </h3>
      <p className="muted small">
        {limits?.max_bytes ? t('PNG·GIF·WebP, 하나에 {size} 이하.', { size: formatBytes(limits.max_bytes) }) : t('PNG·GIF·WebP, 크기 제한 없음.')}{' '}
        {t('이름은 2~32자 글자·숫자·밑줄.')} {t('움직이는 GIF는 애니메이션 이모지가 됩니다.')}
      </p>
      {view.kind === 'crew' && hasPerm(view, 'manage_space') && <EmojiLimitsEditor key={JSON.stringify(limits ?? null)} space={space} limits={limits} />}
      {canManage && (
        <div className="card row gap">
          <button
            type="button"
            className="btn"
            onClick={async () => {
              const f = await client?.transport.pickFiles({ multiple: false, images: true });
              if (f?.[0]) {
                setFile(f[0]);
                if (!name) setName(f[0].name.replace(/\.[^.]+$/, '').replace(/[^\p{L}\p{N}_]/gu, '_').slice(0, 32));
              }
            }}
          >
            <ImagePlus size={15} /> {file ? file.name : t('이미지 고르기')}
          </button>
          <input className={cx('input grow', name && !valid && 'invalid')} placeholder={t('이름(예: yangpa)')} value={name} onChange={(e) => setName(e.target.value)} />
          <button
            type="button"
            className="btn primary"
            disabled={!file || !valid}
            onClick={async () => {
              if (!client || !file) return;
              const r = await call(client.call('emoji.create', { space, name, path: file.path }), { ok: t(':{name}: 이모지를 올렸습니다', { name }) });
              if (r) {
                setFile(null);
                setName('');
                refreshSpace(space);
              }
            }}
          >
            {tc('파일 올리기 단추', '올리기')}
          </button>
        </div>
      )}
      <div className="emoji-grid">
        {view.emojis.map((e) => (
          <div key={e.id} className="emoji-cell">
            <CustomEmoji id={e.id} name={e.name} size={40} />
            <div className="emoji-meta">
              <span className="small">:{e.name}:</span>
              {e.created_by && (
                <span className="emoji-by" title={t('올린 사람')}>
                  <Avatar id={e.created_by} name={displayName(members.get(e.created_by), t('나간 멤버'))} size={16} />
                  <span>{displayName(members.get(e.created_by), t('나간 멤버'))}</span>
                </span>
              )}
            </div>
            {e.animated && <span className="tag">GIF</span>}
            {canManage && (
              <button
                type="button"
                className="icon-btn small danger"
                title={t('삭제')}
                onClick={async () => {
                  if (!client) return;
                  await call(client.call('emoji.delete', { space, id: e.id }));
                  refreshSpace(space);
                }}
              >
                <Trash2 size={13} />
              </button>
            )}
          </div>
        ))}
      </div>
    </div>
  );
}

/** 크루 이모지 한도(크루 관리 권한). 빈 칸은 한도 없음. 줄여도 이미 올린 이모지는 그대로 둡니다. */
function EmojiLimitsEditor({ space, limits }: { space: string; limits: EmojiLimits | undefined }) {
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const [count, setCount] = useState(limits?.max_count != null ? String(limits.max_count) : '');
  const [kib, setKib] = useState(limits?.max_bytes != null ? String(Math.ceil(limits.max_bytes / 1024)) : '');
  const parsed = parseEmojiLimits(count, kib);
  const changed = parsed.ok && (parsed.limits.max_count !== (limits?.max_count ?? null) || parsed.limits.max_bytes !== (limits?.max_bytes ?? null));
  return (
    <div className="card stack">
      <strong>{t('이모지 한도')}</strong>
      <div className="row gap wrap">
        <label className="row gap-s">
          <span className="muted small">{t('최대 개수')}</span>
          <input className={cx('input small', !parsed.ok && count.trim() && 'invalid')} inputMode="numeric" placeholder={String(MAX_EMOJIS)} value={count} onChange={(e) => setCount(e.target.value)} />
        </label>
        <label className="row gap-s">
          <span className="muted small">{t('하나의 최대 크기')}</span>
          <input className={cx('input small', !parsed.ok && kib.trim() && 'invalid')} inputMode="numeric" placeholder={t('무제한')} value={kib} onChange={(e) => setKib(e.target.value)} />
          <span className="muted small">KiB</span>
        </label>
        <button
          type="button"
          className="btn primary"
          disabled={!changed}
          onClick={() => client && parsed.ok && void call(client.call('crew.update', { space, emoji_limits: parsed.limits }), { ok: t('이모지 한도를 저장했습니다') })}
        >
          {t('저장')}
        </button>
      </div>
      <span className="field-hint">{parsed.ok ? t('빈 칸은 한도 없음(개수는 {n}개까지). 줄여도 이미 올린 이모지는 그대로 둡니다.', { n: MAX_EMOJIS }) : <span className="err">{parsed.error}</span>}</span>
    </div>
  );
}

function Seeders({ space }: { space: string }) {
  const view = useStore((s) => s.views[space])!;
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const [cov, setCov] = useState<ChannelCoverage[] | null>(null);
  const [policy, setPolicy] = useState<SeederPolicy | null>(null);
  useEffect(() => {
    if (!client) return;
    let stale = false;
    const load = () => {
      client
        .call('seed.coverage', { space })
        .then((c) => !stale && setCov(c))
        .catch(() => !stale && setCov([]));
    };
    load();
    client
      .call('seed.get', { space })
      .then((p) => !stale && setPolicy(p))
      .catch(() => undefined);
    const timer = setInterval(load, 5000);
    return () => {
      stale = true;
      clearInterval(timer);
    };
  }, [client, space]);
  const channels = view.channels.filter((c) => c.settings.kind !== 'category' && c.settings.kind !== 'voice');
  // 보관 전용 노드도 기록을 내주므로, 시더와 보관 노드가 둘 다 없을 때만 경고합니다.
  const zero = cov?.filter((c) => c.online_seeders === 0 && !c.online_archives) ?? [];
  const excluded = new Set(policy?.exclude_channels ?? []);
  return (
    <div className="stack">
      <h3>{t('시더 커버리지')}</h3>
      <p className="muted small">
        {t('시더는 크루 기록을 들고 있다가 오프라인이었던 멤버에게 나눠 주는 기기입니다(NAS·항상 켜 둔 PC 등). 온라인 시더가 0인 채널은 모두가 꺼져 있을 때 기록을 받을 수 없습니다.')}
      </p>
      {zero.length > 0 && (
        <div className="warn-box">
          <AlertTriangle size={16} />
          <span>
            {t('{n}개 채널에 지금 온라인 시더가 없습니다:', { n: zero.length })}{' '}
            {zero.map((z) => `#${view.channels.find((c) => c.channel === z.channel)?.settings.name ?? '?'}`).join(', ')}
          </span>
        </div>
      )}
      {policy && (
        <div className="card row between">
          <div>
            <strong>{t('이 기기를 이 크루의 시더로 쓰기')}</strong>
            <div className="muted small">{t('켜면 고른 채널 기록을 다른 멤버에게 전달합니다(대역폭 제한은 사용자 설정 → 동기화).')}</div>
          </div>
          <Toggle
            checked={policy.enabled}
            label={t('시더')}
            onChange={async (v) => {
              if (!client) return;
              const r = await call(client.call('seed.set', { space, enabled: v }));
              if (r !== undefined) setPolicy({ ...policy, enabled: v });
            }}
          />
        </div>
      )}
      <div className="table">
        {cov === null && <Spinner />}
        {cov &&
          channels.map((c) => {
            const row = cov.find((x) => x.channel === c.channel);
            const n = row?.online_seeders;
            const archives = row?.online_archives ?? 0;
            const volatile = c.settings.security === 'volatile';
            return (
              <div key={c.channel} className={cx('tr', !volatile && n === 0 && archives === 0 && 'warn-row')}>
                <ChannelIcon channel={c} />
                <span className="grow">{c.settings.name}</span>
                {volatile ? (
                  <span className="muted small">
                    <Flame size={12} /> {t('휘발성: 시딩하지 않음')}
                  </span>
                ) : c.p2p && !c.p2p_joined ? (
                  // 데몬은 참여한 기기끼리만 직접 연결로 셀 수 있어 이 채널을 결과에서 뺍니다.
                  <span className="muted small">{t('P2P 전용: 참여하지 않아 셀 수 없음')}</span>
                ) : (
                  <>
                    {policy && (
                      <label className="check small" title={t('이 기기에서 이 채널을 시딩하지 않음')}>
                        <input
                          type="checkbox"
                          checked={!excluded.has(c.channel)}
                          onChange={async (e) => {
                            if (!client) return;
                            const next = e.target.checked ? [...excluded].filter((x) => x !== c.channel) : [...excluded, c.channel];
                            const r = await call(client.call('seed.set', { space, exclude_channels: next }));
                            if (r !== undefined) setPolicy({ ...policy, exclude_channels: next });
                          }}
                        />
                        {t('내 기기 시딩')}
                      </label>
                    )}
                    <span className={cx('seed-count', n === 0 && archives === 0 && 'zero')}>
                      {n === 0 && archives === 0 ? <AlertTriangle size={13} /> : <Shield size={13} />} {t('온라인 시더 {n}', { n: n ?? '?' })}
                      {archives > 0 && ' · ' + t('보관 노드 {n}', { n: archives })}
                    </span>
                  </>
                )}
              </div>
            );
          })}
      </div>
      <ArchiveNodes space={space} />
    </div>
  );
}
