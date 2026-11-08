import { isTauri, invoke } from '@tauri-apps/api/core';
import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { detectLang, isLang, setLang, t, tc } from './i18n';
import './styles/theme.css';
import './styles/app.css';

/** 저장한 화면 언어(UI 설정 `lang`). 앱 모듈을 불러오기 전이라 클라이언트 대신 설정을 바로 읽습니다(개발 MOCK은 localStorage). */
async function savedLang(): Promise<unknown> {
  try {
    if (isTauri()) return (await invoke<Record<string, unknown>>('ui_prefs_get')).lang;
    return (JSON.parse(localStorage.getItem('yangpa-ui-prefs') ?? '{}') as Record<string, unknown>).lang;
  } catch {
    return undefined;
  }
}

/** 앱 셸이 직접 띄우는 문구(src-tauri/src/i18n.rs의 키). 자리는 셸이 채웁니다. */
function shellTexts(): Record<string, string> {
  return {
    open: t('열기'),
    quit: t('끝내기'),
    // 셸은 복수형을 못 고르므로, 수에 따라 꼴이 바뀌지 않게 옮긴 문구를 씁니다.
    newNotices: tc('셸 알림 요약: 수에 따라 꼴이 바뀌지 않게', '새 알림 {n}개'),
    friendRequest: t('친구 요청'),
    friendRequestBody: t('{name}님이 친구 요청을 보냈습니다'),
    confirmPending: t('AI 요청 확인 필요'),
    newMessage: t('새 메시지'),
    attachment: t('첨부 파일'),
    textFilter: t('텍스트'),
    screen: t('화면 {n}'),
    screenPrimary: t('화면 {n}(주 모니터)'),
    keepAlive: t('메시지를 받기 위해 실행 중입니다'),
  };
}

// 언어를 먼저 정한 뒤 앱을 불러옵니다(모듈 최상위 문구도 t()를 거치게).
async function boot() {
  const saved = await savedLang();
  await setLang(isLang(saved) ? saved : detectLang(navigator.languages)).catch(() => setLang('ko'));
  if (isTauri()) void invoke('shell_texts', { texts: shellTexts() }).catch(() => undefined);
  const { App } = await import('./App');
  const root = document.getElementById('root');
  if (root) {
    createRoot(root).render(
      <StrictMode>
        <App />
      </StrictMode>,
    );
  }
}

void boot();
