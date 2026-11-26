// SPDX-License-Identifier: LicenseRef-YANGPA-Supporter
// 크루 구독자 역할: 확인 경로 이름과 `subrole.updated` 결과 문구.

import type { SubroleResult, SubscriptionProvider } from '../api/types';
import { t } from '../i18n';

export const SUBSCRIPTION_NAMES: Record<SubscriptionProvider, string> = {
  github: 'GitHub Sponsors',
  patreon: 'Patreon',
  sellapp: 'SellApp',
};

export function subroleResultText(r: SubroleResult): string {
  switch (r) {
    case 'linked':
      return t('크리에이터 계정을 연결했습니다.');
    case 'granted':
      return t('구독을 확인해 역할을 받았습니다.');
    case 'not_subscribed':
      return t('이 계정의 정기 구독을 찾지 못했습니다.');
    case 'pending':
      return t('역할을 주는 기기가 확인하고 있습니다. 잠시 뒤 역할이 들어옵니다.');
    case 'invalid':
      return t('확인하지 못한 계정이나 라이선스 키입니다.');
    case 'unreachable':
      return t('역할을 주는 기기에 닿지 못했습니다. 그 기기가 켜져 있을 때 다시 시도하세요.');
    case 'denied':
      return t('로그인을 거절했습니다.');
    case 'expired':
      return t('로그인 시간이 지났습니다. 다시 시도하세요.');
    case 'no_campaign':
      return t('이 Patreon 계정에는 캠페인이 없습니다.');
    case 'too_wide':
      return t('이 계정의 로그인은 계정 확인보다 넓은 권한을 열어 아무것도 보내지 않았습니다. 그 서비스의 연결된 앱 설정에서 YANGPA를 지우고 다시 하거나 다른 계정으로 하세요.');
    case 'relink':
      return t('이 기기에 연결한 Patreon 크리에이터 계정으로는 내 후원 목록을 읽지 못했습니다. 크리에이터 계정을 다시 연결하거나 ‘다른 Patreon 계정으로 확인’을 쓰세요.');
    case 'network':
      return t('확인하지 못했습니다. 연결을 확인하고 다시 시도하세요.');
    default:
      return t('구독 확인에 실패했습니다.');
  }
}

export function subroleToastKind(r: SubroleResult): 'info' | 'success' | 'warn' | 'error' {
  switch (r) {
    case 'linked':
    case 'granted':
      return 'success';
    case 'pending':
      return 'info';
    case 'not_subscribed':
    case 'unreachable':
    case 'relink':
      return 'warn';
    default:
      return 'error';
  }
}
