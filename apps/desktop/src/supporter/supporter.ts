// SPDX-License-Identifier: LicenseRef-YANGPA-Supporter
// 후원자 확인(라이브러리 탭·콘텐츠 뷰어): 확인 경로 이름, 후원 페이지, 로그인 실패 까닭 문구.

import type { SupporterProvider } from '../api/types';
import { t } from '../i18n';

export const PROVIDER_NAMES: Record<SupporterProvider, string> = { github: 'GitHub Sponsors', patreon: 'Patreon' };

/** 후원 페이지. 데몬(crates/yangpa-daemon/src/supporter/mod.rs)의 후원받는 계정과 같게 둡니다. 비어 있으면 버튼을 숨깁니다. */
export const SPONSOR_URLS: Record<SupporterProvider, string> = {
  github: 'https://github.com/sponsors/dhtpdud',
  patreon: 'https://www.patreon.com/VgWorkbench',
};

/** `supporter.updated`의 실패 까닭 코드를 문구로. */
export function supporterErrorText(code: string): string {
  switch (code) {
    case 'denied':
      return t('후원 확인 로그인을 거절했습니다.');
    case 'expired':
      return t('후원 확인 로그인 시간이 지났습니다. 다시 시도하세요.');
    case 'creator':
      return t('크리에이터 본인 계정은 연결할 수 없습니다.');
    case 'network':
      return t('후원 정보를 확인하지 못했습니다. 연결을 확인하고 다시 시도하세요.');
    default:
      return t('후원 확인 로그인에 실패했습니다.');
  }
}
