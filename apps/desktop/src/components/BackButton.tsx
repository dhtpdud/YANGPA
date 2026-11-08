import { ArrowLeft } from 'lucide-react';
import { t } from '../i18n';
import { useStore } from '../store/store';

/** 한 칸씩 보여 주는 좁은 화면(휴대폰)에서만 보이는 뒤로 단추. */
export function BackButton() {
  const back = useStore((s) => s.mobileBack);
  return (
    <button type="button" className="icon-btn tb-back" aria-label={t('뒤로')} title={t('뒤로')} onClick={() => void back()}>
      <ArrowLeft size={22} />
    </button>
  );
}
