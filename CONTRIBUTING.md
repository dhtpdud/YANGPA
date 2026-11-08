# 기여 안내

YANGPA에 관심을 가져 주셔서 고맙습니다. 기여는 [기여자 동의](#라이선스와-기여자-동의)를 따릅니다. 큰 변경은
코드를 쓰기 전에 이슈로 먼저 이야기해 주세요. 보안 문제는 이슈가 아니라 [SECURITY.md](SECURITY.md)의 경로로 알려 주세요.

## 작업 공간 구조

Rust 작업 공간(에디션 2024, resolver 3)과 데스크톱 앱(Tauri 2 + React + TypeScript)으로 되어 있습니다.

```text
crates/
  yangpa-core     공용 타입: 식별자, 서명, 이벤트 DAG, 공간·채널·권한 모델, 링크, 프로토콜 메시지
  yangpa-crypto   AEAD·봉인 상자·잠긴 메모리, 이벤트·블롭 암호화, 신원 키, 키 그룹(키 트리·키 일정)
  yangpa-store    SQLCipher 저장소, 블롭 저장소, 휘발성 RAM 저장소(타입으로 분리)
  yangpa-sync     DAG, 구간 지문 비교, 상태 해소, 시더·피어 정책, 스웜 스케줄러(입출력 없음)
  yangpa-net      libp2p 스웜(Arti 기반 Tor 스웜, P2P 전용 채널·시더용 직접 연결 스웜)
  yangpa-rpc      로컬 JSON-RPC 2.0 프레이밍·타입·클라이언트·서버
  yangpa-daemon   코어 데몬 yangpad (tests/에 노드 여러 개를 띄우는 통합 시험)
  yangpa-cli      CLI yangpa
  yangpa-mcp      MCP 서버 yangpa-mcp
apps/desktop/     데스크톱 앱(src: React, src-tauri: Rust 쪽 yangpa-desktop)
deploy/           헤드리스 시더 예시(docker-compose, systemd)
docs/             기획서와 설계 문서
.github/workflows ci.yml(검사), release.yml(릴리스)
```

- `apps/desktop/src-tauri`(`yangpa-desktop`)는 작업 공간 멤버지만 웹뷰 개발 패키지가 필요해 **기본 멤버가 아닙니다**.
  `cargo build`는 이것을 빼고, `--workspace`나 `-p yangpa-desktop`을 주면 포함합니다.
- 크레이트 경계와 공개 API는 [docs/architecture.md](docs/architecture.md), 데몬 내부 흐름과 모듈 소유는
  [docs/daemon.md](docs/daemon.md)에 있습니다. 경계를 넘는 변경은 이 문서들을 먼저 보세요.

## 설계 문서와 규칙

| 문서 | 내용 |
| --- | --- |
| [docs/plan.md](docs/plan.md) | 기획서 원문(고치지 않음) |
| [docs/architecture.md](docs/architecture.md) | 크레이트 경계·공개 API |
| [docs/protocol.md](docs/protocol.md) | 암호·동기화 |
| [docs/api.md](docs/api.md) | 로컬 JSON-RPC |
| [docs/decisions.md](docs/decisions.md) | 기획서와 달라진 점, 미결정 사항에 넣은 기본값 |

- **계약이 바뀌면 문서도 같은 변경에서 고칩니다.** 공개 API·프로토콜·로컬 API·기본값이 바뀌면 해당 문서를, 기획서와
  달라지거나 기획서가 정하지 않은 것을 정하면 `decisions.md`를 고칩니다. 애매하면 개인정보를 덜 남기는 쪽을 고르고
  그 이유를 적습니다.
- **주석·오류 메시지·문서·UI 문구는 한국어, 식별자는 영어.** 주석은 '무엇'보다 '왜'를 짧게 적습니다.
- **의존성 판은 루트 `Cargo.toml`의 `[workspace.dependencies]`에서만 정합니다.** 크레이트의 `Cargo.toml`은
  `workspace = true`로 가져옵니다. libp2p 0.57, arti 0.47, rusqlite 0.40, rmcp 3.5처럼 새 판이 많으니 API는
  `~/.cargo/registry/src/*/<크레이트>-<판>/`의 실제 소스를 보고 씁니다.
- **휘발성 채널 데이터는 디스크·로그·캐시·알림으로 가는 코드 경로에 절대 들어가면 안 됩니다.** `yangpa-store`가
  휘발성 타입을 저장 타입과 분리해 둔 것을 따르세요. `yangpa diag leakcheck`와 누출 시험이 이를 확인합니다.
- **로그에 메시지 내용·키·토큰을 남기지 않습니다.** 어떤 로그 수준이든 같습니다. 식별자(공간·채널 ID)와 개수·크기는 됩니다.
- 사용자 문서(README, `SECURITY.md`, `CONTRIBUTING.md`, `docs/user-guide.md`, `docs/headless.md`)의 `yangpa …` 예시는
  `crates/yangpa-cli/tests/doc_examples.rs`가 실제 명령줄 구조로 해석해 봅니다. 명령을 바꾸면 문서도 같이 고치세요.

## 코드 규칙

- **MSRV는 Rust 1.91**(`rust-version`)입니다. 1.91보다 새로운 표준 라이브러리 API를 쓰지 말고, 1.99에서 폐기 예정
  (deprecated)인 것도 쓰지 마세요. 릴리스는 1.99.1로 빌드합니다.
- Unix 전용 코드(권한 비트, Unix 소켓, 신호)는 `#[cfg(unix)]`로 감싸고 Windows에서도 빌드되게 합니다. 지금 대상은
  Windows라서 CI는 Windows에서만 돌고, 리눅스 검사는 개발 환경에서 돌립니다.
  Windows 전용 코드(`crates/yangpa-rpc/src/winpipe.rs` 등)는 리눅스에서도
  `CARGO_FEATURE_PURE=1 cargo clippy -p yangpa-rpc --target x86_64-pc-windows-msvc --all-targets`로 미리 확인할 수 있습니다
  (`rustup target add x86_64-pc-windows-msvc`. `CARGO_FEATURE_PURE`는 blake3가 MSVC 어셈블러를 찾지 않게 함).
- `unsafe`는 꼭 필요할 때만, 바로 위에 `// SAFETY:` 주석으로 근거를 적습니다.

## 시험 규칙

- **고정된 시간 대기(`sleep`)로 조건을 기다리지 마세요.** 데몬 시험은 `eventually`·`next_notification`처럼 조건이 될 때까지
  기다리는 도구(`crates/yangpa-daemon/tests/common`, `testkit`)를 씁니다.
- **고정 포트를 쓰지 마세요.** 포트 0으로 열고 받은 주소를 씁니다(다시 띄울 때만 그 주소를 재사용).
- 시험은 외부 네트워크에 의존하지 않습니다. 실제 Tor 망이 필요한 시험은 `#[ignore = "이유"]`로 기본 실행에서 뺍니다
  (`cargo test -- --ignored`로 따로 실행).
- 고친 버그에는 다시 나타나면 실패하는 시험을 붙입니다. 자주 실패하는(flaky) 시험은 같은 변경에서 고칩니다.

## 검사

풀 리퀘스트 전에 다음을 모두 통과시켜 주세요. CI(`ci.yml`)는 이 중 대부분을 Windows에서 다시 돌립니다([직접 등록한 러너](docs/self-hosted-runner.md)).

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# 데스크톱 앱(리눅스는 libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev libxdo-dev 필요)
cd apps/desktop && npm ci && npm run build && npm test
```

- 고치는 동안에는 `-p <크레이트>`로 범위를 좁히면 빠릅니다(예: `cargo test -p yangpa-daemon --test daemon volatile::`). 데몬 통합 시험은 링크를 한 번만 하도록
  `tests/daemon.rs` 하나로 묶여 있어, tests/에 새 파일을 만들면 그 파일에 모듈로 넣습니다.
- 패키징을 바꿨다면: 워크플로는 [actionlint](https://github.com/rhysd/actionlint), 셸 스크립트는
  [shellcheck](https://www.shellcheck.net/), `Dockerfile`은 [hadolint](https://github.com/hadolint/hadolint),
  systemd 단위는 `systemd-analyze verify`로 확인합니다. 설치 훅(MSI 조각·NSIS 훅·deb 유지 스크립트)의 연결은
  `python3 -I scripts/check_installer_hooks.py`로 봅니다.
- 앱 창·온보딩을 바꿨다면 실제 창 연기 시험도 돌립니다(리눅스만. `webkit2gtk-driver`·`xvfb`와
  `cargo install tauri-driver --version 2.1.0 --locked` 필요. 도구나 화면이 없으면 이유를 적고 건너뜀):

  ```bash
  cd apps/desktop
  npm run test:window:build      # 프런트엔드 + yangpad + webdriver 기능을 켠 앱(따로 빌드해 yangpad 기능이 CI와 같음)
  xvfb-run -a npm run test:window
  ```

  빌드한 앱이 옆의 실제 `yangpad`를 띄워 붙고, 화면에서 신원을 만들어 메인 화면까지 갑니다. 데이터·소켓·XDG 경로를
  모두 임시 폴더에 두므로 실제 앱 설정을 건드리지 않습니다. macOS·Windows 창과
  서명·공증, 다른 사람 계정의 데몬은 자동화하지 않으니 [릴리스 절차](#릴리스-절차)의 사람 확인 목록을 보세요.
- 네트워크·통화·미리보기처럼 Tor 경로가 걸린 코드를 바꿨다면 `bash scripts/tor-leak-check.sh --no-tor` 로 같은 시나리오
  (친구·DM·첨부·크루 초대·참여·메시지·통화 거부)를 루프백에서 몇 초 만에 돌려 봅니다. 실제 Tor 망 검사
  ([.github/workflows/tor-leak.yml](.github/workflows/tor-leak.yml))는 매주와 수동 실행으로 돕니다.

## 커밋과 풀 리퀘스트

- 커밋 제목은 짧은 한국어 한 줄로, 무엇을 왜 바꿨는지 적습니다.
- 한 풀 리퀘스트에는 한 가지 일을 담고, 바뀐 계약과 고친 문서를 설명에 적습니다.
- 공유 브랜치의 기록을 다시 쓰지 마세요(force-push 금지).

## 릴리스 절차

릴리스는 [.github/workflows/release.yml](.github/workflows/release.yml)이 `v*` 태그를 받아 만듭니다.

1. 세 곳의 판을 같게 올립니다. 판이 다르거나 태그가 `v<판>`이 아니면 파이프라인이 멈춥니다.
   - `Cargo.toml`의 `[workspace.package] version`(그 뒤 `cargo update --workspace`로 `Cargo.lock`도)
   - `apps/desktop/src-tauri/tauri.conf.json`의 `version`
   - `apps/desktop/package.json`의 `version`(그 뒤 `npm install --package-lock-only`로 `package-lock.json`도)
2. 위 검사를 모두 통과시킨 커밋에 태그를 답니다: `git tag -a v0.2.0 -m "0.2.0"` 후 태그를 올립니다.
   판에 `-`가 들어가면(예: `0.2.0-rc.1`) 시험판(prerelease)으로 표시하고, Windows는 MSI 대신 NSIS 설치 파일만 만듭니다
   (MSI는 숫자로만 된 판을 요구).
3. 파이프라인이 하는 일:
   - 판 번호·사이드카 계약과 설치 훅(`scripts/check_installer_hooks.py`, NSIS 훅 `makensis`, MSI 조각 WiX v3 스키마)을
     긴 빌드보다 먼저 확인
   - 리눅스 x86_64(`ubuntu-22.04`)·aarch64(`ubuntu-22.04-arm`, 둘 다 glibc 2.35 기준), Windows x86_64,
     macOS 유니버설(aarch64·x86_64를 각각 빌드해 `lipo`로 합침)용 `yangpad`·`yangpa`·`yangpa-mcp`를 빌드
   - 데몬을 사이드카(`apps/desktop/src-tauri/binaries/yangpad-<타깃 트리플>[.exe]`, macOS는
     `yangpad-universal-apple-darwin`)로 넣고 Tauri 묶음을 빌드(deb·AppImage, msi·nsis, 유니버설 dmg)
   - 설치 파일을 실제로 설치·업데이트·제거해 봄: Windows는 `scripts/installer-smoke.ps1`(데몬이 도는 채로 NSIS·MSI
     업데이트와 제거, MSI 메이저 업그레이드. MSI는 조각의 동작이 바로 그 데몬을 껐다는 로그 표시까지 봄), 리눅스는 `scripts/installer-smoke.sh deb|appimage`, macOS는
     `installer-smoke.sh dmg`(앱·사이드카가 두 아키텍처를 다 담았는지)
   - 헤드리스 시더 Docker 이미지를 linux/amd64·linux/arm64 각자의 러너에서 빌드해 비루트·읽기 전용으로 띄워 보고
     이미지 파일로 저장
   - 모든 파일과 `SHA256SUMS.txt`를 **초안(draft) 릴리스**에 올림
4. 초안에 손으로 붙이는 파일(APK·포터블 zip)을 올렸으면 `SHA256SUMS.txt`를 그 파일까지 넣어 고친 뒤, 배포자 PC에서
   `scripts/sign-release.sh SHA256SUMS.txt`로 서명해 `SHA256SUMS.txt.sig`를 함께 올립니다(키는 배포자 PC에만, GitHub·CI에
   두지 않음). 앱 업데이트는 이 서명이 맞는 릴리스만 받습니다(decisions.md "앱 업데이트"). 서명 뒤에 목록을 고치면 다시 서명합니다.
5. 초안의 파일과 설명을 확인하고 아래 사람 확인을 마친 뒤 GitHub에서 직접 게시합니다. 이미 게시한 릴리스에는
   파이프라인이 덮어쓰지 않습니다.

자동화하지 않은 것(게시 전에 사람이 확인):

- macOS(Apple 실리콘·Intel 각각)와 Windows에서 실제 창을 띄워 온보딩·DM·통화까지 한 번. 창 자동 시험은 리눅스
  WebKitGTK만 돌립니다(`tauri-driver`는 macOS를 지원하지 않고, Windows용 Edge WebDriver 연결은 아직 CI에 넣지 않음).
- 서명·공증: 서명하지 않으므로 macOS Gatekeeper 경고와 Windows SmartScreen 경고가 사용자 안내와 맞는지.
- 다른 Windows 계정이 띄운 데몬: MSI·NSIS 훅은 자기 계정의 `yangpad`만 끌 수 있습니다(권한 밖). 여러 계정이 쓰는
  PC에서는 다른 계정에서 앱을 먼저 끄라고 안내합니다.
- AppImage는 설치기가 없어 옛 데몬을 끄지 못합니다(앱을 끄고 바꾸라고 안내).
- 실제 Tor 망 검사(`tor-leak.yml`)는 릴리스 파이프라인에 없으니, 네트워크 코드가 바뀐 릴리스는 태그 전에 수동 실행해
  통과를 확인합니다.

코드 서명은 하지 않습니다. 서명 비밀값(Apple 인증서·공증, Windows 인증서)이 생기면 `release.yml`의 "선택: 코드 서명"
주석을 따라 켭니다.

## 라이선스와 기여자 동의

YANGPA는 GNU AGPL v3 이상(`AGPL-3.0-or-later`, [LICENSE](LICENSE))과 7조 추가 조건([NOTICE.md](NOTICE.md))을
따르고, 후원 모듈(`crates/yangpa-daemon/src/supporter/`, `apps/desktop/src/supporter/`)만
[YANGPA 후원 기능 라이선스](LICENSE-SUPPORTER.md)를 따릅니다. 이름과 아이콘은 [TRADEMARK.md](TRADEMARK.md)를 보세요.
라이선스를 바꾸는 변경은 관리자 결정 없이 올리지 마세요.

후원 모듈은 AGPL이 아니라서, 기여를 받으려면 저작권자가 기여물을 두 라이선스 모두로 낼 수 있어야 합니다. 그래서 모든
풀 리퀘스트는 아래 기여자 동의(CLA)를 따릅니다. 풀 리퀘스트 설명에 "기여자 동의에 동의합니다" 또는
"I agree to the CLA"라고 적어 주세요.

### 기여자 동의 (CLA)

기여(풀 리퀘스트·패치·문서 등 YANGPA에 넣으려고 보낸 모든 것)를 보내면 다음에 동의한 것으로 봅니다.

1. 기여의 저작권은 여러분에게 남습니다.
2. 여러분은 Oh Seyoung과 그 승계인에게 기여를 쓰고, 고치고, 다른 것과 묶고, 배포하고, 다른 사람에게 허락하고,
   **어떤 라이선스로든**(AGPL, YANGPA 후원 기능 라이선스, 그 밖의 라이선스) 낼 수 있는 전 세계적이고 영구적이며 무상이고
   취소할 수 없는 비독점 권리를 줍니다. 기여에 들어 있는 여러분의 특허도 같은 조건으로 실시를 허락합니다.
3. 여러분은 기여를 이렇게 허락할 권리가 있습니다. 회사 등 다른 사람이 권리를 가진 부분이 있으면 그 허락을 받았고,
   남의 코드가 들어 있으면 그 출처와 라이선스를 풀 리퀘스트에 적었습니다.
4. 기여는 "있는 그대로" 주며, 여러분은 어떤 보증도 하지 않습니다.

> **Contributor License Agreement (English summary).** By submitting a contribution you keep its copyright and grant
> Oh Seyoung and successors a worldwide, perpetual, royalty-free, irrevocable, non-exclusive license (including a patent
> license) to use, modify, combine, distribute, sublicense and relicense it under any license, including the AGPL and the
> YANGPA Supporter Feature License. You confirm you have the right to grant this, and you give it "as is", without warranty.
