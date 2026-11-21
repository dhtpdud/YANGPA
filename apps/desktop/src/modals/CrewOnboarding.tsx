// 크루 온보딩(디스코드 서버 온보딩처럼): 크루 설정 › 온보딩에서 질문과 선택지(→ 역할)를 정하고, 새 멤버는 들어온 뒤 뜨는
// 창에서 골라 역할을 스스로 받습니다. 채널은 따로 고르지 않고 역할의 채널 권한으로 열립니다(decisions.md "크루 온보딩").

import { ArrowDown, ArrowUp, Plus, Trash2, X } from 'lucide-react';
import { useEffect, useState } from 'react';
import { createPortal } from 'react-dom';
import type { Onboarding, OnboardingOption, OnboardingQuestion, Role, SpaceView } from '../api/types';
import { EVERYONE_ROLE } from '../api/types';
import { EmojiText } from '../components/common';
import { currentLang, LANGS, t } from '../i18n';
import { byLevelDesc, cx, hasPerm, isAdminRole, rankOf, roleColor, roleLevel } from '../lib/misc';
import { MAX_ONBOARDED, useStore } from '../store/store';
import { Field, Modal } from './Modal';

// 데몬 한도(yangpa-core `MAX_ONBOARDING_*`)와 같습니다.
const MAX_QUESTIONS = 15;
const MAX_OPTIONS = 20;
const MAX_OPTION_ROLES = 8;
const MAX_WELCOME = 2000;
const MAX_TEXT = 100;
const MAX_LABEL = 50;

const EMPTY: Onboarding = { welcome: '', questions: [] };

/** 선택지가 주는 역할 전부. */
export function offeredRoles(ob: Onboarding | undefined): Set<string> {
  return new Set(ob?.questions.flatMap((q) => q.options.flatMap((o) => o.roles)) ?? []);
}

/** 지금 역할로 본 답: 선택지의 역할을 모두 가졌으면 고른 것(단일 선택은 앞의 하나만). */
export function currentAnswers(ob: Onboarding, roles: string[]): number[][] {
  return ob.questions.map((q) => {
    const picked = q.options.flatMap((o, i) => (o.roles.length > 0 && o.roles.every((r) => roles.includes(r)) ? [i] : []));
    return q.multiple ? picked : picked.slice(0, 1);
  });
}

/** 번역을 입힌 온보딩: 원문마다 그 언어의 번역문이 있으면 그것, 없으면 원문(질문 순서·역할은 그대로라 답 번호도 같음). */
export function localizeOnboarding(ob: Onboarding, texts: Record<string, string> | undefined): Onboarding {
  if (!texts) return ob;
  const tr = (s: string) => (Object.prototype.hasOwnProperty.call(texts, s) ? texts[s] : s);
  return {
    welcome: tr(ob.welcome),
    questions: ob.questions.map((q) => ({ ...q, title: tr(q.title), options: q.options.map((o) => ({ ...o, label: tr(o.label), description: tr(o.description) })) })),
  };
}

/**
 * 저절로 온보딩 창을 띄울지: 질문이 있는 크루이고, 이 기기에서 마치지 않았고, 선택지 역할을 하나도 갖지 않았으며(다른
 * 기기에서 답함), 설정을 바꿀 수 있는 사람이 아닐 때.
 */
export function needsOnboarding(view: SpaceView, me: string | null, done: string[]): boolean {
  const ob = view.onboarding;
  if (view.kind !== 'crew' || view.disbanded || !ob?.questions.length || done.includes(view.space)) return false;
  if (hasPerm(view, 'manage_space')) return false;
  const offered = offeredRoles(ob);
  const mine = view.members.find((m) => m.user === me)?.roles ?? [];
  return !mine.some((r) => offered.has(r));
}

// 이번 실행에서 ‘나중에’로 닫은 크루(앱을 다시 켜면 다시 띄움).
const dismissed = new Set<string>();

/** 보고 있는 크루에 답할 온보딩이 있으면 창을 띄웁니다(다른 창이 열려 있으면 기다림). */
export function useCrewOnboardingPrompt(view: SpaceView | undefined) {
  const me = useStore((s) => s.me);
  const modalOpen = useStore((s) => s.modal !== null);
  const done = useStore((s) => s.prefs.onboarded);
  useEffect(() => {
    if (!view || modalOpen || dismissed.has(view.space) || !needsOnboarding(view, me, done)) return;
    useStore.getState().openModal({ kind: 'crewOnboarding', space: view.space });
  }, [view, me, modalOpen, done]);
}

/**
 * 멤버가 답하는 창. 지금 가진 역할로 고른 답을 미리 채워, 크루 메뉴에서 다시 열면 답을 바꿀 수 있습니다. 글은 앱 화면
 * 언어의 번역으로 보입니다(`lang`은 설정의 미리 보기용).
 */
export function CrewOnboardingModal({ space, lang, onClose }: { space: string; lang?: string; onClose: () => void }) {
  const view = useStore((s) => s.views[space]);
  const me = useStore((s) => s.me);
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const setPrefs = useStore((s) => s.setPrefs);
  const ob = view?.onboarding;
  const [answers, setAnswers] = useState<number[][]>(() => (ob ? currentAnswers(ob, view.members.find((m) => m.user === me)?.roles ?? []) : []));
  const [busy, setBusy] = useState(false);
  if (!view || !ob?.questions.length) return null;
  const shown = localizeOnboarding(ob, view.onboarding_langs?.[lang ?? currentLang()]);
  // 연 동안 설정이 바뀌었으면 없는 질문·선택지는 버립니다(데몬이 질문 수를 맞춰 봄).
  const sent = ob.questions.map((q, i) => (answers[i] ?? []).filter((n) => n < q.options.length));
  const missing = ob.questions.some((q, i) => q.required && sent[i].length === 0);
  const later = () => {
    dismissed.add(space);
    onClose();
  };
  const toggle = (qi: number, oi: number) =>
    setAnswers(
      ob.questions.map((q, i) => {
        const a = sent[i];
        if (i !== qi) return a;
        if (a.includes(oi)) return a.filter((x) => x !== oi);
        return q.multiple ? [...a, oi] : [oi];
      }),
    );
  const finish = async () => {
    if (!client) return;
    setBusy(true);
    const r = await call(client.call('crew.onboarding_pick', { space, answers: sent }), { ok: t('고른 역할을 받았습니다') });
    setBusy(false);
    if (r === undefined) return;
    const done = useStore.getState().prefs.onboarded.filter((x) => x !== space);
    setPrefs({ onboarded: [...done, space].slice(-MAX_ONBOARDED) });
    onClose();
  };
  return (
    <Modal
      onClose={later}
      className="crew-onboarding"
      title={t('{name}에 오신 것을 환영합니다', { name: view.settings.name })}
      footer={
        <>
          <button type="button" className="btn" onClick={later}>
            {t('나중에')}
          </button>
          <button type="button" className="btn primary" disabled={missing || busy || !client} onClick={() => void finish()}>
            {t('마치기')}
          </button>
        </>
      }
    >
      {shown.welcome && <p className="co-welcome">{shown.welcome}</p>}
      {shown.questions.map((q, qi) => (
        <section key={qi} className="co-question">
          <h4>
            {q.title} {q.required && <span className="tag">{t('필수')}</span>}
          </h4>
          <p className="muted small">{q.multiple ? t('여러 개 고를 수 있습니다') : t('하나만 고릅니다')}</p>
          <div className="co-options">
            {q.options.map((o, oi) => (
              <button
                key={oi}
                type="button"
                className={cx('co-option', sent[qi].includes(oi) && 'active')}
                aria-pressed={sent[qi].includes(oi)}
                onClick={() => toggle(qi, oi)}
              >
                <span className="co-label">
                  <EmojiText text={o.label} />
                </span>
                {o.description && <span className="muted small">{o.description}</span>}
              </button>
            ))}
          </div>
        </section>
      ))}
    </Modal>
  );
}

const newOption = (): OnboardingOption => ({ label: '', description: '', roles: [] });
const newQuestion = (): OnboardingQuestion => ({ title: '', multiple: false, required: false, options: [] });

/** 저장할 수 있는 설정인지(데몬 `check_limits`의 빈 칸 검사와 같음, 길이는 입력칸이 막음). */
export function onboardingComplete(ob: Onboarding): boolean {
  return ob.questions.every(
    (q) => q.title.trim() !== '' && q.options.length > 0 && q.options.every((o) => o.label.trim() !== '' && o.roles.length > 0 && o.roles.length <= MAX_OPTION_ROLES),
  );
}

/** 데몬이 저장하며 고치는 것(앞뒤 공백, 선택지 역할의 순서·겹침)을 뺀 비교 키. */
function obKey(ob: Onboarding): string {
  return JSON.stringify({
    welcome: ob.welcome.trim(),
    questions: ob.questions.map((q) => ({
      ...q,
      title: q.title.trim(),
      options: q.options.map((o) => ({ label: o.label.trim(), description: o.description.trim(), roles: [...new Set(o.roles)].sort() })),
    })),
  });
}

/** 크루 설정 › 온보딩 탭: 디스코드처럼 질문마다 카드, 답변은 타일로 보이고 타일을 누르면 편집 창이 뜹니다. */
export function OnboardingSettings({ space }: { space: string }) {
  const view = useStore((s) => s.views[space])!;
  const me = useStore((s) => s.me);
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const openModal = useStore((s) => s.openModal);
  const saved = view.onboarding ?? EMPTY;
  const savedKey = obKey(saved);
  const [draft, setDraft] = useState<Onboarding>(saved);
  const [baseKey, setBaseKey] = useState(savedKey);
  // 편집 창: 질문 번호와 답변 번호(null이면 새 답변).
  const [editing, setEditing] = useState<{ qi: number; oi: number | null } | null>(null);
  // 다른 관리자·기기가 바꿨을 때 고치던 내용이 없으면 새 값을 따라갑니다.
  if (savedKey !== baseKey) {
    if (obKey(draft) === baseKey) setDraft(saved);
    setBaseKey(savedKey);
  }
  const canManage = hasPerm(view, 'manage_space');
  const dirty = obKey(draft) !== savedKey;
  const complete = onboardingComplete(draft);
  const roleName = new Map(view.roles.map((r) => [r.id, r.name]));
  const setQ = (qi: number, patch: Partial<OnboardingQuestion>) => setDraft({ ...draft, questions: draft.questions.map((q, i) => (i === qi ? { ...q, ...patch } : q)) });
  const move = (qi: number, d: -1 | 1) => {
    const qs = [...draft.questions];
    [qs[qi], qs[qi + d]] = [qs[qi + d], qs[qi]];
    setDraft({ ...draft, questions: qs });
  };
  const editQ = editing ? draft.questions[editing.qi] : undefined;
  // 편집할 언어: ''이면 원문(질문·선택지·역할), 아니면 그 언어의 번역.
  const [lang, setLang] = useState('');
  const langs = view.onboarding_langs ?? {};
  return (
    <div className="stack">
      <h3>{t('온보딩')}</h3>
      <p className="muted small">
        {t('새 멤버는 크루에 들어오면 아래 질문에 답하고, 고른 선택지의 역할을 스스로 받습니다. 채널은 역할의 채널 권한으로 열립니다. 질문이 없으면 꺼집니다.')}
      </p>
      {view.onboarding && (
        <div className="row gap wrap">
          <span className="small">{t('편집할 언어')}</span>
          <select className="select small" value={lang} disabled={dirty} aria-label={t('편집할 언어')} onChange={(e) => setLang(e.target.value)}>
            <option value="">{t('원문')}</option>
            {LANGS.map(([code, name]) => (
              <option key={code} value={code}>
                {langs[code] ? `${name} ✓` : name}
              </option>
            ))}
          </select>
          {view.onboarding.questions.length > 0 && (
            <button type="button" className="btn small" disabled={dirty} onClick={() => openModal({ kind: 'crewOnboarding', space, lang: lang || undefined })}>
              {t('멤버 화면으로 보기')}
            </button>
          )}
          {dirty && <span className="muted small">{t('원문을 저장해야 번역할 수 있습니다')}</span>}
        </div>
      )}
      {lang ? (
        <OnboardingLangEditor key={lang} space={space} lang={lang} />
      ) : (
        <>
          <Field label={t('환영 문구')}>
            <textarea className="input" rows={3} value={draft.welcome} maxLength={MAX_WELCOME} disabled={!canManage} onChange={(e) => setDraft({ ...draft, welcome: e.target.value })} />
          </Field>
          {draft.questions.map((q, qi) => (
            <section key={qi} className="co-qcard">
              <div className="co-qhead">
                <span className="co-qnum">{t('질문 {n}', { n: qi + 1 })}</span>
                {canManage && (
                  <>
                    <button type="button" className="icon-btn small" title={t('위로')} disabled={qi === 0} onClick={() => move(qi, -1)}>
                      <ArrowUp size={14} />
                    </button>
                    <button type="button" className="icon-btn small" title={t('아래로')} disabled={qi === draft.questions.length - 1} onClick={() => move(qi, 1)}>
                      <ArrowDown size={14} />
                    </button>
                  </>
                )}
              </div>
              <input
                className="input"
                placeholder={t('질문')}
                aria-label={t('질문 {n}', { n: qi + 1 })}
                value={q.title}
                maxLength={MAX_TEXT}
                disabled={!canManage}
                onChange={(e) => setQ(qi, { title: e.target.value })}
              />
              <div className="co-qsub">
                {t('준비된 답변')} <b>{`${q.options.length}/${MAX_OPTIONS}`}</b>
              </div>
              <div className="co-options">
                {q.options.map((o, oi) => (
                  <button key={oi} type="button" className={cx('co-option', o.roles.length === 0 && 'warn')} onClick={() => setEditing({ qi, oi })}>
                    <span className="co-label">
                      <EmojiText text={o.label.trim() || t('이름 없는 답변')} />
                    </span>
                    {o.description && <span className="muted small">{o.description}</span>}
                    <span className="co-option-roles small">
                      {o.roles.length ? <EmojiText text={o.roles.map((r) => `@${roleName.get(r) ?? '?'}`).join('  ')} /> : t('역할을 고르세요')}
                    </span>
                  </button>
                ))}
                {canManage && q.options.length < MAX_OPTIONS && (
                  <button type="button" className="co-option co-add" onClick={() => setEditing({ qi, oi: null })}>
                    <Plus size={18} /> {t('답변 추가하기')}
                  </button>
                )}
              </div>
              <div className="co-qfoot">
                <label className="check">
                  <input type="checkbox" checked={q.multiple} disabled={!canManage} onChange={(e) => setQ(qi, { multiple: e.target.checked })} /> {t('여러 개 고르기')}
                </label>
                <label className="check">
                  <input type="checkbox" checked={q.required} disabled={!canManage} onChange={(e) => setQ(qi, { required: e.target.checked })} /> {t('필수')}
                </label>
                <span className="grow" />
                {canManage && (
                  <button
                    type="button"
                    className="icon-btn small danger"
                    title={t('질문 지우기')}
                    onClick={() => setDraft({ ...draft, questions: draft.questions.filter((_, i) => i !== qi) })}
                  >
                    <Trash2 size={16} />
                  </button>
                )}
              </div>
            </section>
          ))}
          {canManage && draft.questions.length < MAX_QUESTIONS && (
            <button type="button" className="co-add co-add-question" onClick={() => setDraft({ ...draft, questions: [...draft.questions, newQuestion()] })}>
              <Plus size={18} /> {t('질문 추가하기')}
            </button>
          )}
          {dirty && canManage && (
            <div className="save-bar">
              <span>{complete ? t('저장하지 않은 변경이 있습니다') : t('질문 제목, 선택지 이름, 선택지마다 역할 하나 이상이 필요합니다')}</span>
              <button type="button" className="btn small" onClick={() => setDraft(saved)}>
                {t('되돌리기')}
              </button>
              <button
                type="button"
                className="btn primary small"
                disabled={!complete || !client}
                onClick={() => client && void call(client.call('crew.set_onboarding', { space, onboarding: draft }), { ok: t('온보딩을 저장했습니다') })}
              >
                {t('저장')}
              </button>
            </div>
          )}
        </>
      )}
      {editing && editQ && (
        <OptionEditor
          view={view}
          me={me}
          qi={editing.qi}
          question={editQ}
          option={editing.oi === null ? undefined : editQ.options[editing.oi]}
          canManage={canManage}
          onClose={() => setEditing(null)}
          onSave={(o) => {
            const { qi, oi } = editing;
            setQ(qi, { options: oi === null ? [...editQ.options, o] : editQ.options.map((x, i) => (i === oi ? o : x)) });
            setEditing(null);
          }}
          onRemove={
            editing.oi === null
              ? undefined
              : () => {
                  const { qi, oi } = editing;
                  setQ(qi, { options: editQ.options.filter((_, i) => i !== oi) });
                  setEditing(null);
                }
          }
        />
      )}
    </div>
  );
}

/** 번역 비교 키: 다듬은 뒤 빈 칸·원문과 같은 칸·지금 원문에 없는 칸을 뺀(데몬이 저장하며 빼는 것) 정렬된 목록. */
function cleanTexts(texts: Record<string, string>, sources: Set<string>): Record<string, string> {
  return Object.fromEntries(
    Object.entries(texts)
      .map(([k, v]) => [k, v.trim()] as const)
      .filter(([k, v]) => v !== '' && v !== k && sources.has(k))
      .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)),
  );
}

/** 온보딩 번역 편집: 원문 칸마다 번역 입력칸(비우면 원문으로 보임). 질문·선택지·역할은 원문에서만 바꿉니다. */
function OnboardingLangEditor({ space, lang }: { space: string; lang: string }) {
  const view = useStore((s) => s.views[space])!;
  const client = useStore((s) => s.client);
  const call = useStore((s) => s.call);
  const ob = view.onboarding ?? EMPTY;
  const sources = new Set([ob.welcome, ...ob.questions.flatMap((q) => [q.title, ...q.options.flatMap((o) => [o.label, o.description])])].filter(Boolean));
  const saved = view.onboarding_langs?.[lang] ?? {};
  const savedKey = JSON.stringify(cleanTexts(saved, sources));
  const [draft, setDraft] = useState<Record<string, string>>(saved);
  const [baseKey, setBaseKey] = useState(savedKey);
  // 다른 관리자·기기가 바꿨을 때 고치던 내용이 없으면 새 값을 따라갑니다.
  if (savedKey !== baseKey) {
    if (JSON.stringify(cleanTexts(draft, sources)) === baseKey) setDraft(saved);
    setBaseKey(savedKey);
  }
  const canManage = hasPerm(view, 'manage_space');
  const texts = cleanTexts(draft, sources);
  const dirty = JSON.stringify(texts) !== savedKey;
  // 같은 원문이 여러 칸에 있으면 한 번역을 함께 씁니다.
  const field = (src: string, max: number, multiline = false) => {
    if (!src) return null;
    const props = { className: 'input', placeholder: src, 'aria-label': src, value: draft[src] ?? '', maxLength: max, disabled: !canManage };
    return (
      <div className="co-tr-row">
        <span className="co-tr-src">{src}</span>
        {multiline ? (
          <textarea {...props} rows={3} onChange={(e) => setDraft({ ...draft, [src]: e.target.value })} />
        ) : (
          <input {...props} onChange={(e) => setDraft({ ...draft, [src]: e.target.value })} />
        )}
      </div>
    );
  };
  return (
    <>
      <p className="muted small">{t('멤버는 앱 화면 언어의 번역을 봅니다. 비운 칸은 원문으로 보입니다.')}</p>
      {ob.welcome && (
        <div className="stack">
          <span className="field-label">{t('환영 문구')}</span>
          {field(ob.welcome, MAX_WELCOME, true)}
        </div>
      )}
      {ob.questions.map((q, qi) => (
        <section key={qi} className="co-qcard">
          <div className="co-qhead">
            <span className="co-qnum">{t('질문 {n}', { n: qi + 1 })}</span>
          </div>
          {field(q.title, MAX_TEXT)}
          <div className="co-qsub">{t('준비된 답변')}</div>
          {q.options.map((o, oi) => (
            <div key={oi} className="co-tr-option">
              {field(o.label, MAX_LABEL)}
              {field(o.description, MAX_TEXT)}
            </div>
          ))}
        </section>
      ))}
      {dirty && canManage && (
        <div className="save-bar">
          <span>{t('저장하지 않은 번역이 있습니다')}</span>
          <button type="button" className="btn small" onClick={() => setDraft(saved)}>
            {t('되돌리기')}
          </button>
          <button
            type="button"
            className="btn primary small"
            disabled={!client}
            onClick={() => client && void call(client.call('crew.set_onboarding_lang', { space, lang, texts }), { ok: t('번역을 저장했습니다') })}
          >
            {t('저장')}
          </button>
        </div>
      )}
    </>
  );
}

/** 답변 하나를 고치는 창(디스코드의 답변 편집처럼 이름·설명과 줄 역할). 저장해야 설정 초안에 들어갑니다. */
function OptionEditor({
  view,
  me,
  qi,
  question,
  option,
  canManage,
  onClose,
  onSave,
  onRemove,
}: {
  view: SpaceView;
  me: string | null;
  qi: number;
  question: OnboardingQuestion;
  option: OnboardingOption | undefined;
  canManage: boolean;
  onClose: () => void;
  onSave: (o: OnboardingOption) => void;
  onRemove?: () => void;
}) {
  const [o, setO] = useState<OnboardingOption>(option ?? newOption());
  // 역할을 넣거나 빼려면 역할 관리 권한도 필요하고, 내 순위와 같거나 높은 역할은 건드리지 못합니다(데몬과 같음).
  const canRoles = hasPerm(view, 'manage_roles');
  const myRank = rankOf(view, me);
  const byId = new Map(view.roles.map((r) => [r.id, r]));
  const roles = view.roles.filter((r) => r.id !== EVERYONE_ROLE).sort(byLevelDesc);
  const locked = (r: Role) => !canManage || !canRoles || roleLevel(r) >= myRank;
  // 관리자 역할은 넣을 수 없습니다(설정 뒤에 관리자가 된 역할은 빼기만).
  const addable = roles.filter((r) => !o.roles.includes(r.id) && !isAdminRole(r) && !locked(r));
  const ok = o.label.trim() !== '' && o.roles.length > 0;
  // 설정 창 안에 그리면 그 창 아래로 깔려서 body에 띄웁니다(Esc는 DOM에서 마지막 창만 닫음).
  return createPortal(
    <Modal
      size="sm"
      className="co-option-editor"
      title={t('질문 {n}', { n: qi + 1 })}
      onClose={onClose}
      footer={
        <>
          {onRemove && canManage && (
            <button type="button" className="btn ghost co-remove" onClick={onRemove}>
              {t('제거하기')}
            </button>
          )}
          <span className="grow" />
          <button type="button" className="btn" onClick={onClose}>
            {t('취소')}
          </button>
          {canManage && (
            <button type="button" className="btn primary" disabled={!ok} onClick={() => onSave(o)}>
              {t('저장')}
            </button>
          )}
        </>
      }
    >
      {question.title.trim() && <p className="co-editor-question">{question.title}</p>}
      <div className="co-editor-step">
        <span className="field-label">{t('1. 답변')}</span>
        <input
          className="input"
          placeholder={t('선택지 이름')}
          aria-label={t('선택지 이름')}
          value={o.label}
          maxLength={MAX_LABEL}
          disabled={!canManage}
          onChange={(e) => setO({ ...o, label: e.target.value })}
        />
        <input
          className="input"
          placeholder={t('설명(선택)')}
          aria-label={t('설명(선택)')}
          value={o.description}
          maxLength={MAX_TEXT}
          disabled={!canManage}
          onChange={(e) => setO({ ...o, description: e.target.value })}
        />
      </div>
      <div className="co-editor-step">
        <span className="field-label">{t('2. 역할 지정')}</span>
        <div className="co-role-box">
          {o.roles.map((id) => {
            const r = byId.get(id);
            return (
              <span key={id} className="co-role-chip">
                <span className="co-role-dot" style={{ background: (r && roleColor(r.color)) || 'var(--text-muted)' }} />
                <EmojiText text={`@${r?.name ?? '?'}`} />
                {(!r || !locked(r)) && (
                  <button type="button" className="co-role-x" title={t('빼기')} onClick={() => setO({ ...o, roles: o.roles.filter((x) => x !== id) })}>
                    <X size={12} />
                  </button>
                )}
              </span>
            );
          })}
          {addable.length > 0 && o.roles.length < MAX_OPTION_ROLES && (
            <select className="co-role-add" aria-label={t('역할 추가…')} value="" onChange={(e) => e.target.value && setO({ ...o, roles: [...o.roles, e.target.value] })}>
              <option value="">{t('역할 추가…')}</option>
              {addable.map((r) => (
                <option key={r.id} value={r.id}>
                  {r.name}
                </option>
              ))}
            </select>
          )}
        </div>
        <span className="field-hint">
          {roles.length === 0 ? t('줄 수 있는 역할이 없습니다. 역할·권한 탭에서 먼저 만드세요.') : t('멤버는 이 역할로 열리는 채널을 모두 보게 됩니다.')}
        </span>
      </div>
    </Modal>,
    document.body,
  );
}
