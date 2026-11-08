# YANGPA 후원 기능 라이선스 1.0 (`LicenseRef-YANGPA-Supporter`)

저작권 © 2026 Oh Seyoung. 모든 권리는 아래에서 허락한 것을 빼고 저작권자에게 있습니다.

영어 번역은 아래에 있습니다. 두 판의 뜻이 다르면 이 한국어 판을 따릅니다.

## 적용 범위

이 라이선스는 다음 파일("후원 모듈")에 적용됩니다.

- `crates/yangpa-daemon/src/supporter/` 폴더의 파일
- `apps/desktop/src/supporter/` 폴더의 파일
- 첫 줄에 `SPDX-License-Identifier: LicenseRef-YANGPA-Supporter`가 있는 파일

YANGPA의 나머지 코드는 GNU AGPL 3판 이상([LICENSE](LICENSE), [NOTICE.md](NOTICE.md))을 따릅니다. 후원 모듈은 AGPL을
따르지 않습니다.

"후원 확인"은 GitHub Sponsors·Patreon으로 Oh Seyoung을 유효하게 후원하지 않는 사용자에게 후원 기능(라이브러리 탭,
콘텐츠 뷰어, 후원자 표시 등)을 잠가 두는 동작 전체를 말합니다. 후원 모듈 안의 확인 코드뿐 아니라, 그 결과로 기능을
잠그고 여는 코드가 어느 파일에 있든(AGPL 부분의 `supporterOk`·`selectLibrary`·`haloAllowed`·`BannerBack` 등 포함) 여기에
들어갑니다. 후원한 적이 있어야 주는 표시와 뱃지(한 번이라도 후원한 금액에 따른 ☕🍕💎 뱃지와, 그 금액을 `badges.rs`의
`SUPPORT_CENTS` 기준으로 계산하는 코드 포함)를 그 후원 없이 주지 않는 동작도 후원 확인에 들어갑니다.

"배포"는 후원 모듈이나 그것을 담은 프로그램(소스·실행 파일·설치 파일)을 자기 자신이 아닌 사람에게 주거나, 내려받게
하거나, 네트워크로 쓰게 하는 것입니다. 가족·회사·단체 안에서 주는 것도 배포입니다.

## 1. 허락하는 것

1. 후원 모듈의 소스를 읽고, 보안을 감사하고, 그 결과를 발표할 수 있습니다.
2. 후원 모듈을 빌드해 **자기 자신만** 쓸 수 있습니다. 자기만 쓰려고 고치는 것(후원 확인을 바꾸는 것 포함)도 됩니다.
3. 후원 모듈을 YANGPA나 YANGPA를 고친 판에 넣어 배포할 수 있습니다. 단 다음을 모두 지켜야 합니다.
   - 이 라이선스 파일과 저작권 표시, 파일의 `SPDX-License-Identifier` 줄을 그대로 둡니다.
   - 배포하는 프로그램 전체에서 후원 확인이 원본과 같은 효과로 동작합니다. 이 조건 안에서 버그를 고치거나 다른
     코드에 맞춘 판도 됩니다.
4. 고친 내용을 원 저장소에 기여할 수 있습니다([CONTRIBUTING.md](CONTRIBUTING.md)의 기여자 동의를 따름).

## 2. 금지하는 것

1. 후원 모듈을 담은 프로그램에서, 어느 파일을 고쳐서든 후원 확인을 없애거나, 우회하거나, 늘 통과하게 하거나, 다른
   후원 대상이나 결제 수단으로 바꿔서 유효한 후원 없이 후원 기능이 열리게 한 판을 배포하는 것. 무료든 유료든, 실행
   파일이든 소스든 같습니다. 그렇게 고치는 패치·스크립트를 배포하는 것도 같습니다.
2. 후원 모듈을 팔거나, 대가를 받고 후원 기능을 열어 주는 것.
3. 이 라이선스나 저작권 표시를 지우거나 바꾸는 것.

## 3. 권리의 끝

이 라이선스를 어기면 여기서 받은 권리는 자동으로 끝납니다. 저작권자가 위반을 알린 날부터 30일 안에 위반을 멈추고
배포한 것을 거두면, 처음 한 번에 한해 권리가 다시 살아납니다.

## 4. 보증 없음

후원 모듈은 "있는 그대로" 제공하며 어떤 보증도 하지 않습니다. 법이 허용하는 한 저작권자는 후원 모듈을 쓰거나 쓰지
못해 생긴 어떤 손해에도 책임지지 않습니다.

## 5. 준거법

이 라이선스는 대한민국 법을 따릅니다.

---

# YANGPA Supporter Feature License 1.0 (English translation)

Copyright © 2026 Oh Seyoung. All rights reserved except as granted below.

This is a translation. If it differs from the Korean text above, the Korean text prevails.

## Scope

This license applies to the following files (the "Supporter Module"):

- files in `crates/yangpa-daemon/src/supporter/`
- files in `apps/desktop/src/supporter/`
- any file whose first line contains `SPDX-License-Identifier: LicenseRef-YANGPA-Supporter`

The rest of YANGPA is licensed under the GNU AGPL version 3 or later ([LICENSE](LICENSE), [NOTICE.md](NOTICE.md)). The
Supporter Module is not licensed under the AGPL.

"Supporter Check" means the whole behavior that keeps supporter features (the Library tab, the content viewer,
supporter flair and so on) locked for users without a valid sponsorship of Oh Seyoung through GitHub Sponsors or Patreon.
It covers not only the checking code in the Supporter Module but also the code that locks or unlocks features based on
the result, in whatever file it lives (including `supporterOk`, `selectLibrary`, `haloAllowed`, `BannerBack` and the
like in the AGPL parts). It also covers not granting, without the corresponding sponsorship, marks and badges that
require having sponsored (including the ☕🍕💎 badges for any amount ever sponsored, and the code in `badges.rs` that
computes them against `SUPPORT_CENTS`).

"Distribute" means giving the Supporter Module, or a program containing it (as source, executable or installer), to
anyone other than yourself, letting them download it, or letting them use it over a network. Giving it within a family,
company or organization is also distribution.

## 1. Permissions

1. You may read the Supporter Module's source, audit its security and publish the results.
2. You may build the Supporter Module and use it **yourself only**. You may modify it for your own use, including
   changing the Supporter Check.
3. You may distribute the Supporter Module as part of YANGPA or a modified version of YANGPA, provided that:
   - this license file, the copyright notice and each file's `SPDX-License-Identifier` line stay intact; and
   - the Supporter Check keeps working with the same effect as in the original, across the whole distributed program.
     Within this condition, versions with bug fixes or adaptations to other code are allowed.
4. You may contribute changes to the original repository (under the contributor agreement in
   [CONTRIBUTING.md](CONTRIBUTING.md)).

## 2. Prohibitions

1. Distributing a program containing the Supporter Module in which the Supporter Check, by changes to any file, is
   removed, bypassed, made to always pass, or redirected to another recipient or payment method, so that supporter
   features unlock without a valid sponsorship. This applies
   whether free or paid, and to executables and source alike. Distributing patches or scripts that make such a change is
   also prohibited.
2. Selling the Supporter Module, or unlocking supporter features in exchange for payment.
3. Removing or altering this license or the copyright notice.

## 3. Termination

If you violate this license, your rights under it end automatically. If you stop the violation and withdraw what you
distributed within 30 days of the copyright holder notifying you, your rights are reinstated, once only.

## 4. No warranty

The Supporter Module is provided "as is", without warranty of any kind. To the extent permitted by law, the copyright
holder is not liable for any damages arising from the use of, or inability to use, the Supporter Module.

## 5. Governing law

This license is governed by the laws of the Republic of Korea.
