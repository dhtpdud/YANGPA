// 권한 템플릿 줄(역할 편집·채널 권한 공용): 기본·내 템플릿을 불러와 저장 전 폼만 바꾸고, 지금 설정을 이름 붙여 UI 설정에 둡니다.

import { X } from 'lucide-react';
import { useState } from 'react';
import { t } from '../i18n';
import { BUILTIN_TEMPLATES, MAX_TEMPLATES, MAX_TEMPLATE_NAME, isBuiltinTemplate, removeTemplate, templateName, upsertTemplate, type PermTemplate } from '../lib/perms';
import { useStore } from '../store/store';

export function PermTemplateBar({
  scope,
  hint,
  saveLabel,
  current,
  onLoad,
}: {
  /** 저장해야 적용되는 곳(역할·채널·카테고리). */
  scope: string;
  hint: string;
  saveLabel: string;
  /** 지금 설정(템플릿으로 저장할 값). */
  current: { permissions: string[]; deny?: string[] };
  /** 템플릿을 폼에 쓰고, 내게 없어 건너뛴 권한 수를 돌려줍니다. */
  onLoad: (tpl: PermTemplate) => number;
}) {
  const templates = useStore((s) => s.prefs.permTemplates);
  const setPrefs = useStore((s) => s.setPrefs);
  const [name, setName] = useState('');
  const [note, setNote] = useState<string | null>(null);
  const n = templateName(name);
  const exists = templates.some((x) => x.name === n);
  const problem = !n
    ? null
    : isBuiltinTemplate(n)
      ? t('기본 템플릿과 같은 이름은 쓸 수 없습니다.')
      : !exists && templates.length >= MAX_TEMPLATES
        ? t('템플릿은 {n}개까지 저장할 수 있습니다.', { n: MAX_TEMPLATES })
        : null;

  const load = (tpl: PermTemplate) => {
    const skipped = onLoad(tpl);
    const loaded = t('「{name}」 템플릿을 불러왔습니다. 저장해야 {scope}에 적용됩니다.', { name: tpl.name, scope });
    setNote(skipped > 0 ? loaded + ' ' + t('내게 없는 권한 {n}개는 그대로 뒀습니다.', { n: skipped }) : loaded);
  };
  const save = () => {
    if (!n || problem) return;
    setPrefs({ permTemplates: upsertTemplate(templates, n, current.permissions, current.deny) });
    setName('');
    setNote(exists ? t('「{name}」 템플릿을 덮어썼습니다.', { name: n }) : t('「{name}」 템플릿을 저장했습니다.', { name: n }));
  };
  const remove = (tpl: PermTemplate) => {
    setPrefs({ permTemplates: removeTemplate(templates, tpl.name) });
    setNote(t('「{name}」 템플릿을 지웠습니다.', { name: tpl.name }));
  };

  return (
    <div className="perm-group">
      <div className="perm-title">{t('권한 템플릿')}</div>
      <div className="chips">
        <span className="muted small tpl-label">{t('기본')}</span>
        {BUILTIN_TEMPLATES.map((tpl) => (
          <button key={tpl.name} type="button" className="chip" aria-label={t('템플릿 불러오기: {name}', { name: tpl.name })} onClick={() => load(tpl)}>
            {tpl.name}
          </button>
        ))}
      </div>
      {templates.length > 0 && (
        <div className="chips mt">
          <span className="muted small tpl-label">{t('내 템플릿')}</span>
          {templates.map((tpl) => (
            <span key={tpl.name} className="chip tpl-chip">
              <button type="button" aria-label={t('템플릿 불러오기: {name}', { name: tpl.name })} onClick={() => load(tpl)}>
                {tpl.name}
              </button>
              <button type="button" className="tpl-x" title={t('지우기')} aria-label={t('템플릿 지우기: {name}', { name: tpl.name })} onClick={() => remove(tpl)}>
                <X size={12} />
              </button>
            </span>
          ))}
        </div>
      )}
      <div className="row gap mt">
        <input
          className="input small grow"
          aria-label={t('템플릿 이름')}
          placeholder={t('새 템플릿 이름')}
          value={name}
          maxLength={MAX_TEMPLATE_NAME}
          onChange={(e) => setName(e.target.value)}
          onKeyDown={(e) => e.key === 'Enter' && save()}
        />
        <button type="button" className="btn small" disabled={!n || !!problem} onClick={save}>
          {exists ? t('덮어쓰기') : saveLabel}
        </button>
      </div>
      <span className="field-hint mt">{problem ?? note ?? hint}</span>
    </div>
  );
}
