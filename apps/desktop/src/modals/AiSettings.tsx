// 설정 › AI 번역: 내 AI API 프로필(`ai.profiles`·`ai.save_profile`·`ai.delete_profile`·`ai.models`).
// API 키는 데몬이 DB에 두고 앱으로 다시 보내지 않습니다. 고칠 때 키 칸을 비우면 그대로 둡니다.

import { Languages, Pencil, Trash2 } from 'lucide-react';
import { useCallback, useEffect, useState } from 'react';
import type { AiApi, AiProfileView } from '../api/types';
import { Spinner } from '../components/common';
import { resetTranslateFailures } from '../hooks/useTranslated';
import { t } from '../i18n';
import { AI_PRESETS, apiName } from '../lib/aiTr';
import { useStore } from '../store/store';
import { Field } from './Modal';

const APIS: AiApi[] = ['openai', 'anthropic', 'gemini'];

/** 내 프로필 목록(번역 팝업·크루 역할 설정도 씀). */
export function useOwnAiProfiles(): [AiProfileView[] | null, () => Promise<void>] {
  const client = useStore((s) => s.client);
  const [list, setList] = useState<AiProfileView[] | null>(null);
  const load = useCallback(async () => {
    if (client) setList(await client.call('ai.profiles', {}).catch(() => []));
  }, [client]);
  useEffect(() => {
    void load();
  }, [load]);
  return [list, load];
}

interface Draft {
  id?: string;
  name: string;
  api: AiApi;
  base_url: string;
  model: string;
  key: string;
  has_key: boolean;
}

const EMPTY: Draft = { name: '', api: 'openai', base_url: '', model: '', key: '', has_key: false };

export function AiTab() {
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const openModal = useStore((s) => s.openModal);
  const [list, load] = useOwnAiProfiles();
  const [draft, setDraftState] = useState<Draft | null>(null);
  const [busy, setBusy] = useState(false);
  const [models, setModels] = useState<string[] | null>(null);
  const [loadingModels, setLoadingModels] = useState(false);
  const set = (patch: Partial<Draft>) => {
    setDraftState((d) => (d ? { ...d, ...patch } : d));
    // 다른 서비스의 목록이 남지 않게.
    if ('api' in patch || 'base_url' in patch) setModels(null);
  };
  const setDraft = (d: Draft | null) => {
    setDraftState(d);
    setModels(null);
  };

  // 저장 전 칸의 형식·주소·키로 묻습니다(키 칸이 비었으면 저장한 키로).
  const loadModels = async () => {
    if (!client || !draft) return;
    setLoadingModels(true);
    const r = await call(
      client.call('ai.models', { id: draft.id, api: draft.api, base_url: draft.base_url.trim(), key: draft.key ? draft.key : undefined }),
    );
    setLoadingModels(false);
    if (r) setModels(r);
  };

  const save = async () => {
    if (!client || !draft) return;
    setBusy(true);
    const r = await call(
      client.call('ai.save_profile', {
        id: draft.id,
        name: draft.name.trim(),
        api: draft.api,
        base_url: draft.base_url.trim(),
        model: draft.model.trim(),
        key: draft.key ? draft.key : undefined,
      }),
      { ok: t('AI 프로필을 저장했습니다') },
    );
    setBusy(false);
    if (r) {
      setDraft(null);
      resetTranslateFailures();
      void load();
    }
  };

  const remove = (p: AiProfileView) =>
    openModal({
      kind: 'confirm',
      title: t('AI 프로필 지우기'),
      body: t("'{name}' 프로필과 저장한 API 키를 이 기기에서 지웁니다. 크루에 내준 프로필은 크루 설정에서 따로 빼야 합니다.", { name: p.name }),
      danger: true,
      confirmLabel: t('지우기'),
      onConfirm: async () => {
        if (!client) return;
        await call(client.call('ai.delete_profile', { id: p.id }), { ok: t('AI 프로필을 지웠습니다') });
        void load();
      },
    });

  return (
    <div className="stack">
      <h3>{t('AI API 프로필')}</h3>
      <p className="muted small">
        {t(
          '채널 위 번역 버튼(문자 아이콘)에서 고를 AI API를 등록합니다. 번역할 글은 고른 서비스로 보내지고(Tor로 도는 기기는 Tor를 거쳐), 키는 이 기기 데이터베이스에만 암호화해 둡니다.',
        )}
      </p>
      <div className="table">
        {list === null && <Spinner />}
        {list?.length === 0 && <div className="muted small">{t('등록한 프로필이 없습니다.')}</div>}
        {list?.map((p) => (
          <div key={p.id} className="tr">
            <Languages size={18} />
            <div className="grow">
              <div className="row gap-s">
                <strong>{p.name}</strong>
                <span className="tag">{apiName(p.api)}</span>
                {!p.has_key && <span className="tag">{t('키 없음')}</span>}
              </div>
              <div className="muted small mono break">
                {p.base_url} · {p.model}
              </div>
            </div>
            <button type="button" className="icon-btn" title={t('고치기')} onClick={() => setDraft({ ...p, key: '' })}>
              <Pencil size={16} />
            </button>
            <button type="button" className="icon-btn danger" title={t('지우기')} onClick={() => remove(p)}>
              <Trash2 size={16} />
            </button>
          </div>
        ))}
      </div>
      {!draft && (
        <div>
          <button type="button" className="btn small primary" onClick={() => setDraft(EMPTY)}>
            {t('프로필 추가')}
          </button>
        </div>
      )}
      {draft && (
        <div className="card stack">
          <Field label={t('서비스')} hint={t('고르면 API 형식과 주소를 채웁니다. 다른 OpenAI 호환 서비스는 주소를 직접 적으세요.')}>
            <select
              className="input"
              aria-label={t('서비스')}
              value=""
              onChange={(e) => {
                const p = AI_PRESETS.find((x) => x.name === e.target.value);
                if (p) set({ api: p.api, base_url: p.base_url, model: draft.model || p.model, name: draft.name || p.name });
              }}
            >
              <option value="">{t('직접 입력')}</option>
              {AI_PRESETS.map((p) => (
                <option key={p.name} value={p.name}>
                  {p.name}
                </option>
              ))}
            </select>
          </Field>
          <div className="grid2">
            <Field label={t('이름')}>
              <input className="input" aria-label={t('이름')} value={draft.name} maxLength={128} onChange={(e) => set({ name: e.target.value })} />
            </Field>
            <Field label={t('API 형식')}>
              <select className="input" aria-label={t('API 형식')} value={draft.api} onChange={(e) => set({ api: e.target.value as AiApi })}>
                {APIS.map((a) => (
                  <option key={a} value={a}>
                    {apiName(a)}
                  </option>
                ))}
              </select>
            </Field>
          </div>
          <Field label={t('API 주소 (Base URL)')} hint={t('https 주소만 됩니다(이 컴퓨터·내부망 주소는 막습니다).')}>
            <input
              className="input mono"
              aria-label={t('API 주소 (Base URL)')}
              value={draft.base_url}
              maxLength={512}
              placeholder="https://api.openai.com/v1"
              spellCheck={false}
              onChange={(e) => set({ base_url: e.target.value })}
            />
          </Field>
          <div className="grid2">
            <Field label={t('모델')}>
              <div className="row gap-s">
                <input
                  className="input mono grow"
                  aria-label={t('모델')}
                  value={draft.model}
                  maxLength={128}
                  spellCheck={false}
                  onChange={(e) => set({ model: e.target.value })}
                />
                <button
                  type="button"
                  className="btn small"
                  disabled={loadingModels || !draft.base_url.trim()}
                  onClick={() => void loadModels()}
                >
                  {loadingModels ? <Spinner /> : t('모델 목록 불러오기')}
                </button>
              </div>
              {models && (
                <>
                  <select
                    className="input mono ai-models"
                    size={Math.min(8, Math.max(2, models.length))}
                    aria-label={t('모델 목록')}
                    value={models.includes(draft.model) ? draft.model : ''}
                    onChange={(e) => set({ model: e.target.value })}
                  >
                    {models.map((m) => (
                      <option key={m} value={m}>
                        {m}
                      </option>
                    ))}
                  </select>
                  <span className="field-hint">{t('{n}개 모델을 찾았습니다. 목록에서 고르세요.', { n: models.length })}</span>
                </>
              )}
            </Field>
            <Field label={t('API 키')} hint={draft.has_key ? t('비워 두면 저장한 키를 그대로 씁니다.') : undefined}>
              <input
                className="input mono"
                type="password"
                autoComplete="off"
                aria-label={t('API 키')}
                value={draft.key}
                maxLength={1024}
                placeholder={draft.has_key ? '••••••••' : ''}
                spellCheck={false}
                onChange={(e) => set({ key: e.target.value })}
              />
            </Field>
          </div>
          <div className="row gap">
            <button
              type="button"
              className="btn small primary"
              disabled={busy || !draft.name.trim() || !draft.base_url.trim() || !draft.model.trim()}
              onClick={() => void save()}
            >
              {t('저장')}
            </button>
            <button type="button" className="btn small" onClick={() => setDraft(null)}>
              {t('취소')}
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
