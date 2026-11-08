import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// Tauri 개발 서버 규약: 고정 포트, 화면 지우기 끔, TAURI_ 접두 환경 변수 노출.
const host = process.env.TAURI_DEV_HOST;
// 개발 빌드(tauri dev)에서만 켜지는 표식. 배포본에서는 꺼져, 아래에서 디버깅 흔적을 뺍니다.
const isDebug = !!process.env.TAURI_ENV_DEBUG;

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  envPrefix: ['VITE_', 'TAURI_ENV_'],
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host ? { protocol: 'ws', host, port: 1421 } : undefined,
    watch: { ignored: ['**/src-tauri/**'] },
  },
  worker: { format: 'es' },
  // 배포 번들은 읽기 어렵게 합니다(원본 코드 노출을 줄여 리버스 엔지니어링 비용을 높임). 보안을 번들의 비밀에
  // 기대지는 않습니다(상대가 개조한 클라이언트를 쓸 수 있다는 전제는 그대로) — 다만 불필요한 단서를 남기지 않습니다.
  define: {
    // 배포 Tauri 번들 빌드(tauri build)일 때만 true: Tauri가 TAURI_ENV_PLATFORM을 넣고, 릴리스는 TAURI_ENV_DEBUG가 없음.
    // 개발 서버·스크린샷용 `npm run build`(Tauri 밖)에는 이 값이 false라, 데몬 없이 화면을 띄우는 MOCK 백엔드가 그대로 들어갑니다.
    __YANGPA_SHIPPED__: JSON.stringify(!!process.env.TAURI_ENV_PLATFORM && !process.env.TAURI_ENV_DEBUG),
  },
  build: {
    target: 'es2022',
    // 배포본은 식별자 축약·주석·공백 제거로 최소화합니다(기본값이지만 강화 의도를 드러내 명시). CSS도 함께 줄입니다.
    minify: true,
    cssMinify: true,
    // 소스맵은 개발 빌드에서만(배포본에 원본 코드·원본 위치를 싣지 않음).
    sourcemap: isDebug,
    chunkSizeWarningLimit: 1500,
  },
});
