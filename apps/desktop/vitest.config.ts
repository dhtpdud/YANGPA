import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    environment: 'node',
    // 컴포넌트 시험(.test.tsx)은 파일 첫 줄의 `@vitest-environment happy-dom`으로 DOM을 씁니다.
    include: ['src/**/*.test.ts', 'src/**/*.test.tsx'],
  },
});
