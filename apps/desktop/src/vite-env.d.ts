/// <reference types="vite/client" />

// 배포 Tauri 번들 빌드일 때만 true로 치환됩니다(vite.config.ts의 define). 개발·스크린샷 빌드에서는 false,
// 치환되지 않는 곳(테스트)에서는 선언만 있고 값이 없어 쓰는 쪽에서 typeof로 기본값(배포 아님)을 둡니다.
// 이 값으로 배포본에서 MOCK 백엔드(api/mock.ts)를 통째로 빼, 전체 API 표면이 읽기 쉬운 형태로 실리지 않게 합니다.
declare const __YANGPA_SHIPPED__: boolean;
