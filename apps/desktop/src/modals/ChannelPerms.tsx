// 채널·카테고리 권한(디스코드식): 대상(@everyone·역할·멤버)마다 권한을 거부·기본·허용으로 덮어씁니다.
// 비공개 스위치와 역할 칩은 같은 덮어쓰기 목록을 보는 창입니다(@everyone 채널 보기 거부 + 역할 채널 보기 허용).
// 권한 템플릿은 고른 대상 하나에만 씁니다(허용·거부, 이 채널 종류에서 보이는 권한만).

import { Check, Lock, Slash, Trash2, User, X } from 'lucide-react';
import { useState, type ReactNode } from 'react';
import type { ChannelKind, MemberView, OverwriteTarget, PermissionOverwrite, SpaceView } from '../api/types';
import { EVERYONE_ROLE } from '../api/types';
import { EmojiText, Toggle } from '../components/common';
import { currentLang, t } from '../i18n';
import { emojiPlain } from '../lib/emoji';
import { PERMISSION_LABELS, byLevelDesc, canGrant, cx, displayName, roleColor } from '../lib/misc';
import {
  EVERYONE_TARGET,
  addTarget,
  applyTemplateToTarget,
  channelPermGroups,
  isPrivate,
  removeTarget,
  setPrivate,
  setTriState,
  targetKey,
  targetTemplate,
  toggleViewRole,
  triState,
  viewRoles,
  type TriState,
} from '../lib/perms';
import { Field } from './Modal';
import { PermTemplateBar } from './PermTemplates';

const TRI: Array<[TriState, string, ReactNode]> = [
  ['deny', t('거부'), <X key="deny" size={14} />],
  ['inherit', t('기본'), <Slash key="inherit" size={12} />],
  ['allow', t('허용'), <Check key="allow" size={14} />],
];

const byName = (a: MemberView, b: MemberView) => displayName(a).localeCompare(displayName(b), currentLang());

export function ChannelPermsEditor({
  view,
  kind,
  overwrites,
  onChange,
}: {
  view: SpaceView;
  kind: ChannelKind;
  overwrites: PermissionOverwrite[];
  onChange: (next: PermissionOverwrite[]) => void;
}) {
  const [sel, setSel] = useState(targetKey(EVERYONE_TARGET));
  const roles = view.roles.filter((r) => r.id !== EVERYONE_ROLE).sort(byLevelDesc);
  const listed = new Set(overwrites.map((o) => targetKey(o.target)));
  // 내게 보이지 않는 역할의 항목은 목록에 그대로 두되(저장할 때 지우지 않게) 화면에는 보이지 않습니다.
  const targets: OverwriteTarget[] = [
    EVERYONE_TARGET,
    ...roles.filter((r) => listed.has(`role:${r.id}`)).map((r): OverwriteTarget => ({ type: 'role', id: r.id })),
    ...view.members
      .filter((m) => listed.has(`member:${m.user}`))
      .sort(byName)
      .map((m): OverwriteTarget => ({ type: 'member', id: m.user })),
  ];
  const target = targets.find((x) => targetKey(x) === sel) ?? EVERYONE_TARGET;
  const everyone = target.type === 'role' && target.id === EVERYONE_ROLE;
  const addRoles = roles.filter((r) => !listed.has(`role:${r.id}`));
  const addMembers = view.members.filter((m) => !listed.has(`member:${m.user}`)).sort(byName);
  const label = (x: OverwriteTarget) =>
    x.type === 'member' ? displayName(view.members.find((m) => m.user === x.id)) : x.id === EVERYONE_ROLE ? '@everyone' : (view.roles.find((r) => r.id === x.id)?.name ?? '?');
  const dot = (x: OverwriteTarget) =>
    x.type === 'member' ? (
      <User size={13} />
    ) : (
      <span className="role-dot" style={{ background: roleColor(view.roles.find((r) => r.id === x.id)?.color ?? 0) ?? 'var(--text-muted)' }} />
    );

  const priv = isPrivate(overwrites);
  const chosen = viewRoles(overwrites);
  const canView = canGrant(view, 'view_channel');
  const category = kind === 'category';
  const scope = category ? t('카테고리') : t('채널');
  const groups = channelPermGroups(kind);
  const shown = groups.flatMap(([, list]) => list);

  return (
    <>
      <Field
        label={
          <span className="row gap-s">
            <Lock size={13} /> {category ? t('비공개 카테고리') : t('비공개 채널')}
          </span>
        }
        hint={
          priv && chosen.length === 0
            ? t('역할을 고르지 않으면 관리자와 채널 보기를 허용한 멤버만 봅니다.')
            : (category
                ? priv
                  ? t('고른 역할(과 관리자)만 이 카테고리를 봅니다.')
                  : t('켜면 고른 역할(과 관리자)만 이 카테고리를 봅니다.')
                : priv
                  ? t('고른 역할(과 관리자)만 이 채널을 봅니다.')
                  : t('켜면 고른 역할(과 관리자)만 이 채널을 봅니다.')) +
              ' ' +
              t('아래 @everyone의 ‘채널 보기’ 거부와 같은 설정입니다.')
        }
      >
        <Toggle checked={priv} disabled={!canView} onChange={(v) => onChange(setPrivate(overwrites, v))} label={t('비공개')} />
        {priv && (
          <div className="chips mt">
            {roles.length === 0 && <span className="muted small">{t('역할이 없습니다. 크루 설정에서 만드세요.')}</span>}
            {roles.map((r) => (
              <button
                key={r.id}
                type="button"
                disabled={!canView}
                className={cx('chip', chosen.includes(r.id) && 'active')}
                onClick={() => onChange(toggleViewRole(overwrites, r.id))}
              >
                <span className="role-dot" style={{ background: roleColor(r.color) ?? 'var(--text-muted)' }} />
                <EmojiText text={r.name} />
              </button>
            ))}
          </div>
        )}
      </Field>

      <div className="field-label">{t('역할·멤버별 권한')}</div>
      <div className="perm-layout">
        <div className="perm-targets">
          {targets.map((x) => (
            <button
              key={targetKey(x)}
              type="button"
              className={cx('perm-target', targetKey(x) === targetKey(target) && 'active')}
              aria-pressed={targetKey(x) === targetKey(target)}
              onClick={() => setSel(targetKey(x))}
            >
              {dot(x)}
              <span className="grow">
                <EmojiText text={label(x)} />
              </span>
            </button>
          ))}
          {(addRoles.length > 0 || addMembers.length > 0) && (
            <select
              className="select small"
              aria-label={t('역할·멤버 추가')}
              value=""
              onChange={(e) => {
                const [type, id] = e.target.value.split(':');
                if (!id) return;
                const added: OverwriteTarget = type === 'member' ? { type: 'member', id } : { type: 'role', id };
                onChange(addTarget(overwrites, added));
                setSel(targetKey(added));
              }}
            >
              <option value="">{t('+ 역할·멤버 추가')}</option>
              {addRoles.length > 0 && (
                <optgroup label={t('역할')}>
                  {addRoles.map((r) => (
                    <option key={r.id} value={`role:${r.id}`}>
                      {emojiPlain(r.name)}
                    </option>
                  ))}
                </optgroup>
              )}
              {addMembers.length > 0 && (
                <optgroup label={t('멤버')}>
                  {addMembers.map((m) => (
                    <option key={m.user} value={`member:${m.user}`}>
                      {displayName(m)}
                    </option>
                  ))}
                </optgroup>
              )}
            </select>
          )}
        </div>
        <div className="perm-grid">
          <div className="row between">
            <strong className="row gap-s">
              {dot(target)} <EmojiText text={label(target)} />
            </strong>
            <button
              type="button"
              className="btn ghost small"
              onClick={() => {
                onChange(removeTarget(overwrites, target));
                if (!everyone) setSel(targetKey(EVERYONE_TARGET));
              }}
            >
              {everyone ? (
                t('모두 기본으로')
              ) : (
                <>
                  <Trash2 size={13} /> {t('이 설정 지우기')}
                </>
              )}
            </button>
          </div>
          <PermTemplateBar
            key={targetKey(target)}
            scope={scope}
            saveLabel={t('지금 설정을 템플릿으로 저장')}
            hint={t('불러오면 이 대상의 권한만 바뀌고, 저장해야 {scope}에 적용됩니다.', { scope })}
            current={targetTemplate(overwrites, target, shown)}
            onLoad={(tpl) => {
              const r = applyTemplateToTarget(overwrites, target, shown, tpl, (p) => canGrant(view, p));
              onChange(r.overwrites);
              return r.skipped;
            }}
          />
          {groups.map(([group, list]) => (
            <div key={group} className="perm-group">
              <div className="perm-title">{group}</div>
              {list.map((p) => (
                <div key={p} className="perm-row">
                  <span>{PERMISSION_LABELS[p]}</span>
                  <TriSwitch
                    label={PERMISSION_LABELS[p]}
                    value={triState(overwrites, target, p)}
                    disabled={!canGrant(view, p)}
                    onChange={(v) => onChange(setTriState(overwrites, target, p, v))}
                  />
                </div>
              ))}
            </div>
          ))}
        </div>
      </div>
      <span className="field-hint">
        {t(
          '거부(✕)·기본(／)·허용(✓). @everyone → 역할(역할끼리 겹치면 허용이 이김) → 멤버 순으로 적용합니다. 채널 보기를 거부하면 다른 권한도 없고, 관리자는 늘 모든 권한을 가집니다. 내게 없는 권한은 바꿀 수 없습니다.',
        )}
      </span>
    </>
  );
}

function TriSwitch({ label, value, disabled, onChange }: { label: string; value: TriState; disabled: boolean; onChange: (v: TriState) => void }) {
  return (
    <div className="tri" role="radiogroup" aria-label={label} title={disabled ? t('내게 없는 권한은 바꿀 수 없습니다') : undefined}>
      {TRI.map(([v, name, icon]) => (
        <button
          key={v}
          type="button"
          role="radio"
          aria-checked={value === v}
          aria-label={`${label} ${name}`}
          title={disabled ? undefined : name}
          disabled={disabled}
          className={cx('tri-btn', v, value === v && 'active')}
          onClick={() => onChange(v)}
        >
          {icon}
        </button>
      ))}
    </div>
  );
}
