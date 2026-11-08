import { fileURLToPath } from 'node:url';
import { defineConfig } from 'vitest/config';

// 실제 창 연기 시험만(`npm run test:window`). 일반 `npm test`에는 들어가지 않습니다.
export default defineConfig({
  root: fileURLToPath(new URL('..', import.meta.url)),
  test: {
    environment: 'node',
    include: ['e2e-window/**/*.test.ts'],
    testTimeout: 240_000,
    hookTimeout: 60_000,
    // 창·드라이버 포트·데몬은 하나씩만.
    fileParallelism: false,
    // 건너뛴 이유(빠진 도구)가 보고서에 그대로 보이게.
    reporters: ['verbose'],
  },
});
