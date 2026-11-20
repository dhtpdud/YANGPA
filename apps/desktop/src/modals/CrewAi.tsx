// 크루 AI 번역 설정: 역할 편집의 ‘이 역할에 내주는 AI 프로필’(`crew.set_ai_profiles`). 저장 막대를 거치지 않고
// 바로 적용합니다. 누가 번역을 쓸지는 권한 ‘AI 번역 사용’(역할 권한·채널 덮어쓰기)입니다.

import { AlertTriangle, Languages } from 'lucide-react';
import { useState } from 'react';
import type { Role, RoleId, SpaceId, SpaceView } from '../api/types';
import { LANGS, t } from '../i18n';
import { apiName } from '../lib/aiTr';
import { hasPerm } from '../lib/misc';
import { useStore } from '../store/store';
import { useOwnAiProfiles } from './AiSettings';
import { Field } from './Modal';

/** 역할 편집 안: 이 역할을 가진 멤버가 쓸 수 있는 크루 AI 프로필. 크루 설정 권한(`manage_space`)이 있을 때만 보입니다. */
export function RoleAiProfiles({ space, role }: { space: string; role: Role }) {
  const view = useStore((s) => s.views[space]);
  if (!view || view.kind !== 'crew' || !hasPerm(view, 'manage_space')) return null;
  return <RoleAiProfilesEdit space={space} role={role} view={view} />;
}

function RoleAiProfilesEdit({ space, role, view }: { space: string; role: Role; view: SpaceView }) {
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const [own] = useOwnAiProfiles();
  const [busy, setBusy] = useState(false);
  const crew = view.ai_profiles ?? [];
  const base = crew.map((p) => ({ id: p.id, roles: p.roles }));
  const addable = (own ?? []).filter((o) => !crew.some((c) => c.id === o.id));

  // 모든 프로필을 보냅니다. 받는 역할이 하나도 남지 않은 것은 데몬이 뺍니다(내게 보이지 않는 역할은 데몬이 그대로 둠).
  const submit = async (next: { id: string; roles: RoleId[] }[]) => {
    if (!client) return;
    setBusy(true);
    await call(client.call('crew.set_ai_profiles', { space, profiles: next }), { ok: t('AI 프로필 제공을 바꿨습니다') });
    setBusy(false);
  };
  const toggle = (id: string, on: boolean) =>
    void submit(base.map((p) => (p.id === id ? { ...p, roles: on ? [...p.roles, role.id] : p.roles.filter((r) => r !== role.id) } : p)));

  return (
    <div className="perm-group">
      <div className="perm-title">{t('AI 번역 프로필')}</div>
      <span className="field-hint">{t('이 역할을 가진 멤버가 번역 팝업에서 고를 수 있는 AI API 프로필입니다. 내 프로필을 내주면 멤버의 번역 요청을 이 기기가 받아 대신 번역하고, 키는 이 기기 밖으로 보내지 않습니다.')}</span>
      {crew.length === 0 && <span className="muted small">{t('크루가 내준 AI 프로필이 없습니다.')}</span>}
      {crew.map((p) => (
        <div key={p.id} className="perm-row">
          <span>
            {p.name} <span className="muted small">{`${apiName(p.api)} · ${p.model}`}</span>
          </span>
          <input
            type="checkbox"
            aria-label={p.name}
            checked={p.roles.includes(role.id)}
            disabled={busy}
            onChange={(e) => toggle(p.id, e.target.checked)}
          />
        </div>
      ))}
      {addable.length > 0 && (
        <div className="row gap-s">
          <select
            className="input grow"
            aria-label={t('내 프로필 내주기')}
            value=""
            disabled={busy || crew.length >= 16}
            onChange={(e) => e.target.value && void submit([...base, { id: e.target.value, roles: [role.id] }])}
          >
            <option value="">{t('내 프로필 내주기…')}</option>
            {addable.map((o) => (
              <option key={o.id} value={o.id} disabled={!o.has_key}>
                {o.has_key ? o.name : t('{name} (키 없음)', { name: o.name })}
              </option>
            ))}
          </select>
        </div>
      )}
      <div className="warn-box small">
        <AlertTriangle size={14} />
        <div>
          {t(
            '멤버가 번역할 글은 이 기기를 거쳐 AI 서비스로 가고, 번역 요금은 키 주인에게 청구됩니다. 이 기기가 켜져 있어야 멤버가 번역할 수 있습니다.',
          )}
        </div>
      </div>
    </div>
  );
}

/**
 * 크루 설정 › 개요: 멤버 자동 번역. 크루 언어를 고르면 앱 화면 언어가 그와 다르고 크루 AI 프로필을 쓸 수 있는 멤버는
 * 따로 끄지 않았으면 번역이 켜집니다(`crew.set_ai_auto`). 크루 설정 권한이 있을 때만 보입니다.
 */
export function CrewAiAutoField({ space }: { space: SpaceId }) {
  const view = useStore((s) => s.views[space]);
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const [busy, setBusy] = useState(false);
  if (!view || view.kind !== 'crew' || !hasPerm(view, 'manage_space')) return null;
  const noProfile = !view.ai_profiles?.length;
  const set = async (lang: string) => {
    if (!client) return;
    setBusy(true);
    await call(client.call('crew.set_ai_auto', { space, lang: lang || null }), {
      ok: lang ? t('멤버 자동 번역을 켰습니다') : t('멤버 자동 번역을 껐습니다'),
    });
    setBusy(false);
  };
  return (
    <Field
      label={
        <span className="row gap-s">
          <Languages size={13} /> {t('멤버 자동 번역')}
        </span>
      }
      hint={
        noProfile && !view.ai_auto_lang
          ? t('먼저 역할 편집에서 AI 번역 프로필을 내주세요.')
          : t('크루 AI 프로필을 쓸 수 있는 멤버 중 앱 화면 언어가 크루 언어가 아닌 사람은 처음부터 번역이 켜집니다. 멤버는 번역 팝업에서 언제든 끌 수 있습니다.')
      }
    >
      <select
        className="input"
        aria-label={t('크루 언어')}
        value={view.ai_auto_lang ?? ''}
        disabled={busy || (noProfile && !view.ai_auto_lang)}
        onChange={(e) => void set(e.target.value)}
      >
        <option value="">{t('끔')}</option>
        {LANGS.map(([code, name]) => (
          <option key={code} value={code}>
            {t('크루 언어: {lang}', { lang: name })}
          </option>
        ))}
      </select>
    </Field>
  );
}
