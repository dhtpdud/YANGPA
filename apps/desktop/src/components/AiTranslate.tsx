// 채널 위 막대의 AI 번역 버튼과 팝업: 프로필(내 것 + 이 크루가 내준 것) · 원본 언어 · 번역할 언어 · 이 크루 번역.
// 고른 값은 Prefs.aiTr에 두고, 번역 자체는 hooks/useTranslated.ts가 보이는 글마다 합니다.

import { AlertTriangle, Info, Languages, RotateCcw } from 'lucide-react';
import { useState, type ReactNode } from 'react';
import type { ChannelView, SpaceView } from '../api/types';
import { clearTranslateCache } from '../hooks/useTranslated';
import { t } from '../i18n';
import { apiName, autoOn, channelAllowed, defaultTarget, langFor, MAX_LANG, profileFor, trOn, usableCrewProfiles, type AiTrLang } from '../lib/aiTr';
import { cx } from '../lib/misc';
import { useOwnAiProfiles } from '../modals/AiSettings';
import { useStore } from '../store/store';
import { Popover, Toggle } from './common';

export function AiTranslateButton({ space, channel }: { space: SpaceView; channel?: ChannelView }) {
  const [anchor, setAnchor] = useState<DOMRect | null>(null);
  const on = useStore((s) => trOn(s.prefs.aiTr, space, s.identity?.user));
  return (
    <>
      <button
        type="button"
        className={cx('icon-btn', on && 'active')}
        title={t('AI 번역')}
        aria-label={t('AI 번역')}
        onClick={(e) => setAnchor(anchor ? null : e.currentTarget.getBoundingClientRect())}
      >
        <Languages size={20} />
      </button>
      {anchor && (
        <Popover anchor={anchor} onClose={() => setAnchor(null)} className="ai-tr-pop">
          <AiTranslatePanel space={space} channel={channel} />
        </Popover>
      )}
    </>
  );
}

function AiTranslatePanel({ space, channel }: { space: SpaceView; channel?: ChannelView }) {
  const prefs = useStore((s) => s.prefs.aiTr);
  const setPrefs = useStore((s) => s.setPrefs);
  const openModal = useStore((s) => s.openModal);
  const me = useStore((s) => s.identity?.user);
  const [own] = useOwnAiProfiles();
  const [details, setDetails] = useState(false);
  const id = space.space;
  const crew = usableCrewProfiles(space, me);
  const picked = profileFor(prefs, space, me);
  const pickedCrew = picked.startsWith('crew:') ? crew.find((c) => `crew:${c.id}` === picked) : undefined;
  const crewOnly = !!prefs.crewLang[id];
  const lang = langFor(prefs, id);
  const isCrew = space.kind === 'crew';
  const on = trOn(prefs, space, me);
  // 크루 버튼은 막지 않습니다(크루 역할에 없어도 채널 덮어쓰기로 허용한 채널이 있을 수 있음).
  const blocked = !channelAllowed(space, channel, me);
  const volatileBlocked = channel?.settings.security === 'volatile' && !channel.api_read_allowed;
  const noProfile = own !== null && own.length === 0 && crew.length === 0;

  const patch = (p: Partial<typeof prefs>) => setPrefs({ aiTr: { ...prefs, ...p } });
  const setLang = (l: Partial<AiTrLang>) => {
    const next = { source: lang.source, auto: lang.auto, target: lang.target, ...l };
    if (crewOnly) patch({ crewLang: { ...prefs.crewLang, [id]: next } });
    else patch(next);
  };
  const pick = (ref: string) => {
    const crewProfile = { ...prefs.crewProfile, [id]: ref };
    // 내 프로필은 다른 크루·대화에서도 기본으로 씁니다.
    patch(ref.startsWith('own:') ? { crewProfile, profile: ref } : { crewProfile });
    setDetails(false);
  };

  return (
    <div className="ai-tr stack">
      <div className="ai-tr-head">
        <Languages size={16} />
        <strong>{t('AI 번역')}</strong>
      </div>

      <label className="field">
        <span className="field-label">{t('AI API 프로필')}</span>
        <div className="row gap-s">
          <select className="input grow" aria-label={t('AI API 프로필')} value={picked} onChange={(e) => pick(e.target.value)}>
            <option value="" disabled>
              {t('프로필을 고르세요')}
            </option>
            {!!own?.length && (
              <optgroup label={t('내 프로필')}>
                {own.map((p) => (
                  <option key={p.id} value={`own:${p.id}`}>
                    {p.name}
                  </option>
                ))}
              </optgroup>
            )}
            {crew.length > 0 && (
              <optgroup label={isCrew ? t('크루 제공') : t('제공받은 프로필')}>
                {crew.map((p) => (
                  <option key={p.id} value={`crew:${p.id}`}>
                    {p.name}
                  </option>
                ))}
              </optgroup>
            )}
          </select>
          {pickedCrew && (
            <button type="button" className={cx('btn small', details && 'active')} onClick={() => setDetails(!details)}>
              <Info size={14} /> {t('자세히 보기')}
            </button>
          )}
        </div>
      </label>
      {pickedCrew && (
        <span className="field-hint">{t('번역할 글은 이 프로필을 내준 멤버의 기기를 거쳐 AI 서비스로 갑니다. 그 기기가 켜져 있어야 번역됩니다.')}</span>
      )}
      {pickedCrew && details && (
        <dl className="ai-tr-details small">
          <dt>{t('API 형식')}</dt>
          <dd>{apiName(pickedCrew.api)}</dd>
          <dt>{t('API 주소 (Base URL)')}</dt>
          <dd className="mono break">{pickedCrew.base_url}</dd>
          <dt>{t('모델')}</dt>
          <dd className="mono break">{pickedCrew.model}</dd>
          <dt>{t('받는 역할')}</dt>
          <dd>{pickedCrew.roles.map((r) => space.roles.find((x) => x.id === r)?.name ?? '?').join(', ')}</dd>
        </dl>
      )}
      {noProfile && (
        <div className="muted small">
          {t('등록한 프로필이 없습니다.')}{' '}
          <button type="button" className="link-btn" onClick={() => openModal({ kind: 'userSettings', tab: 'ai' })}>
            {t('설정에서 프로필 추가')}
          </button>
        </div>
      )}

      <LangInput
        label={t('원본 언어')}
        value={lang.auto ? '' : lang.source}
        placeholder={t('자동 감지')}
        disabled={lang.auto}
        onCommit={(v) => setLang({ source: v })}
        extra={
          <label className="row gap-s small ai-tr-auto">
            <input type="checkbox" checked={lang.auto} onChange={(e) => setLang({ auto: e.target.checked })} />
            {t('자동 감지')}
          </label>
        }
      />
      <LangInput
        label={t('번역할 언어')}
        value={lang.target}
        placeholder={defaultTarget()}
        onCommit={(v) => setLang({ target: v })}
        hint={t('어떤 언어든 직접 입력하세요. 예: 한국어 · English · Tiếng Việt · 쉬운 한국어 · 존댓말')}
      />
      <label className="row gap-s small">
        <input
          type="checkbox"
          checked={crewOnly}
          onChange={(e) => {
            const crewLang = { ...prefs.crewLang };
            if (e.target.checked) crewLang[id] = { source: prefs.source, auto: prefs.auto, target: prefs.target };
            else delete crewLang[id];
            patch({ crewLang });
          }}
        />
        {isCrew ? t('이 크루에서만 쓰는 언어로 저장') : t('이 대화에서만 쓰는 언어로 저장')}
      </label>

      {blocked && (
        <div className="warn-box small">
          <AlertTriangle size={14} />
          <div>{channel ? t('이 채널에서는 AI 번역을 쓸 수 있는 역할이 아닙니다') : t('이 크루에서는 AI 번역을 쓸 수 있는 역할이 아닙니다')}</div>
        </div>
      )}
      {volatileBlocked && (
        <div className="warn-box small">
          <AlertTriangle size={14} />
          <div>
            {t("휘발성 채널은 'AI 읽기 허용'을 켠 기기만 번역합니다(번역할 글이 AI 서비스로 나갑니다).")}{' '}
            {t('켜면 이 기기의 API 토큰(AI·자동화 포함)도 이 채널을 읽을 수 있습니다.')}
          </div>
        </div>
      )}
      <button type="button" className={cx('btn', on ? '' : 'primary')} disabled={!picked} onClick={() => patch({ always: { ...prefs.always, [id]: !on } })}>
        {on ? (isCrew ? t('이 크루 번역 끄기') : t('이 대화 번역 끄기')) : isCrew ? t('이 크루 번역하기') : t('이 대화 번역하기')}
      </button>
      {isCrew && <span className="field-hint">{t('켜면 이 크루의 어느 채널에 들어가도 자동으로 번역합니다.')}</span>}
      {isCrew && prefs.always[id] === undefined && autoOn(space, me) && (
        <span className="field-hint">{t('이 크루 관리자가 멤버 자동 번역을 켜 두어 크루 AI 프로필로 번역합니다.')}</span>
      )}
      <div className="row between gap">
        <span className="small">{t('채널·크루 이름도 번역')}</span>
        <Toggle checked={prefs.names} label={t('채널·크루 이름도 번역')} onChange={(v) => patch({ names: v })} />
      </div>
      <button
        type="button"
        className="btn small"
        title={t('이 기기에 저장한 번역 결과를 지우고 보이는 글을 다시 번역합니다.')}
        onClick={async () => {
          const { client, call } = useStore.getState();
          if (!client) return;
          // 데몬 DB의 캐시를 먼저 비워야 다시 묻는 글이 옛 번역을 받지 않습니다.
          if ((await call(client.call('ai.clear_cache', {}), { ok: t('번역 캐시를 비웠습니다') })) === undefined) return;
          clearTranslateCache();
        }}
      >
        <RotateCcw size={13} /> {t('번역 캐시 초기화')}
      </button>
    </div>
  );
}

/** 언어 입력(칸을 떠나거나 Enter일 때만 저장: 쓰는 중의 글로 번역을 보내지 않게). */
function LangInput({
  label,
  value,
  placeholder,
  disabled,
  hint,
  extra,
  onCommit,
}: {
  label: string;
  value: string;
  placeholder: string;
  disabled?: boolean;
  hint?: string;
  extra?: ReactNode;
  onCommit: (v: string) => void;
}) {
  const [draft, setDraft] = useState<string | null>(null);
  const commit = () => {
    if (draft !== null && draft.trim() !== value) onCommit(draft.trim());
    setDraft(null);
  };
  return (
    <div className="field">
      <span className="field-label">{label}</span>
      <div className="row gap-s">
        <input
          className="input grow"
          aria-label={label}
          value={draft ?? value}
          placeholder={placeholder}
          disabled={disabled}
          maxLength={MAX_LANG}
          onChange={(e) => setDraft(e.target.value)}
          onBlur={commit}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && !e.nativeEvent.isComposing) commit();
          }}
        />
        {extra}
      </div>
      {hint && <span className="field-hint">{hint}</span>}
    </div>
  );
}
