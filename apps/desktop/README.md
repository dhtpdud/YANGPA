# 양파 메신저(가칭) 데스크톱 앱

Tauri 2 + React + TypeScript(Vite). 화면은 웹뷰, 데이터·암호·네트워크는 전부 백그라운드 데몬(`yangpad`)이 맡습니다.
앱은 데몬의 로컬 JSON-RPC(docs/api.md)를 그대로 부르는 얇은 GUI입니다.

## 실행

```sh
npm ci
npm run dev          # 브라우저에서 http://localhost:1420 — 데몬 없이 MOCK 백엔드(api/mock.ts)로 동작
npm run build        # tsc --noEmit + vite build → dist/
npm test             # vitest: 단위·컴포넌트(happy-dom)·계약 시험 + 실제 데몬 종단 간 시험(아래)
npm run screenshot   # dist를 MOCK으로 띄워 screenshots/에 1440x900 영어 화면 저장(SHOT_LANG=ko면 한국어 화면을 screenshots/ko/에. 전역 Playwright 사용, 없으면 PLAYWRIGHT_MODULE·CHROME_PATH로 지정)

# 데스크톱 앱(리눅스는 webkit2gtk-4.1, libsoup-3.0 개발 패키지 필요)
cargo build -p yangpa-desktop        # 작업 공간 기본 멤버가 아니므로 -p로 지정
npm run tauri dev                    # Vite 개발 서버 + 앱
```

데몬 없이 실제 앱(IPC·연결 상태·재연결·사이드카 띄우기)을 확인하는 연기 시험:

```sh
cargo build -p yangpa-desktop --features custom-protocol --example smoke_daemon --bin yangpa-desktop
export YANGPA_ENDPOINT=/tmp/smoke.sock YANGPA_TOKEN=smoke
YANGPA_DAEMON_BIN=target/debug/examples/smoke_daemon target/debug/yangpa-desktop   # 온보딩 화면까지 오면 정상
```

### 사이드카(배포 묶음의 데몬)

`tauri.conf.json`의 `bundle.externalBin`은 `["binaries/yangpad"]`입니다. 배포 파이프라인이 `yangpad`를 빌드해
`src-tauri/binaries/yangpad-<타깃 트리플>`(윈도는 `.exe`)로 넣으면 `npm run tauri build`가 앱 옆에 함께 묶습니다
(묶을 때 트리플은 떨어지고 `yangpad`가 됨). `binaries/`는 git이 무시합니다(README만 둠).
사이드카가 없으면 `build.rs`가 `TAURI_CONFIG`로 `externalBin`을 빼고 빌드해, 개발 중에도 `cargo build -p yangpa-desktop`이
그대로 됩니다(이때는 아래 "데몬 연결"의 순서로 데몬을 찾음).

### 화면 언어(src/i18n.ts)

화면 문구는 `t('한국어 원문', { 자리 })`·`tn(…)`(자리에 React 노드)으로 씁니다. 원문이 곧 키이고, 번역은
`src/locales/<언어>.json`(값은 문자열, 또는 `{n}` 자리가 있으면 `{one, few, many, other}` 복수형)에 둡니다. 언어는 UI 설정
`lang`(없으면 OS 언어)으로 앱 모듈을 불러오기 전에 정하므로 모듈 최상위 상수에도 `t()`를 그대로 씁니다. 데몬·셸의 글자 그대로인
오류 문구는 `toRpcError`가 같은 카탈로그로 번역합니다. `src/i18n.test.ts`가 t() 밖 한글(화면에 안 뜨는 줄은 `// i18n-ignore: 이유`),
언어마다 빠지거나 남은 키, `{자리}` 이름 불일치를 막습니다. 문구를 더하거나 바꾸면 14개 카탈로그에 번역도 더하세요.
한 파일만 볼 때: `I18N_FILES=modals/Crew.tsx npx vitest run src/i18n.test.ts -t "화면 문구는"`.
같은 원문을 화면마다 달리 옮겨야 하면 `tc('문맥', '원문')`(키는 `원문\u0004문맥`). 카탈로그 파일을 새로 더하거나 지운 뒤에는
vitest 디스크 캐시가 `import.meta.glob` 결과를 옛것으로 쓸 수 있어 `node_modules/.vitest-cache`를 지우고 돌립니다.

### 계약 검사(데몬과 어긋나지 않게)

`src/api/methods.generated.json`은 `cargo test -p yangpa-rpc --test desktop_contract`가 데몬 메서드 표(이름·범위·공간 규칙·
파괴적 여부·사람 전용)·알림 종류·권한 비트에서 만듭니다. 표가 바뀌었는데 파일이 낡았으면 파일을 새로 쓰고 실패합니다
(새 파일을 커밋하면 통과). `src/api/contract.test.ts`는 이 파일과 `types.ts`의 `Methods`·`Notification`·`PERMISSION_BITS`,
권한 이름표가 정확히 같은지 봅니다. 데몬 쪽이 함께 들어오는 중인 메서드는 `PENDING_*`에 적어 두면, 생성 파일에 들어올 때
적은 범위·규칙과 같은지만 검사합니다(지금은 모더레이션과 기기 복구 유예·확정 기기 지정). 그런 영역을 합친 뒤에는
`cargo test -p yangpa-rpc --test desktop_contract`를 한 번 돌려 새로 쓴 생성 파일을 커밋하고, 들어온 항목은 `PENDING_*`에서 뺍니다.

### 실제 데몬 종단 간 시험(src/e2e)

`npm test`는 `yangpad` 2~3개를 임시 폴더·`127.0.0.1`·mDNS 끔으로 띄우고, 앱의 API 클라이언트(`ApiClient`)를 Node 전송
(`src/api/nodeTransport.ts`, 유닉스 소켓·윈도 파이프)에 붙여 신원·연락처 링크(임시 연락처 → 친구 추가·수락)·양방향 메시지·크루 초대와 3자 대화·휘발성 채널과
만료·첨부 왕복·차단·신고·밴을 확인합니다. 데몬 실행 파일은 `YANGPA_YANGPAD` → `CARGO_TARGET_DIR` → 작업 공간 `target/{debug,release}`
순서로 찾고, 없으면 이유를 적고 건너뜁니다(`YANGPA_E2E_REQUIRED=1`이면 실패, CI가 이렇게 돌림). 모더레이션이 없는 예전 데몬이면
그 시나리오만 건너뜁니다.

```sh
cargo build -p yangpa-daemon
npx vitest run src/e2e     # 종단 간 시험만
```

### 실제 창 연기 시험(e2e-window, 리눅스)

빌드한 앱을 Xvfb 위의 실제 Tauri 창으로 띄워 WebDriver로 몹니다: `tauri-driver` → `WebKitWebDriver`가 앱을 띄우고,
앱은 옆의 `yangpad`를 스스로 띄워 붙습니다(임시 `YANGPA_DATA_DIR`·`YANGPA_ENDPOINT`). 시험은 화면에서 신원을 만들고
메인 화면(레일·사이드바, 오류 띠·모의 모드 띠 없음)을 본 뒤, 같은 데몬에
Node 전송으로 붙어 `identity.get`의 이름이 맞는지 확인합니다. 클라이언트는
`e2e-window/webdriver.ts`(fetch만 쓰는 최소 W3C 클라이언트)라 npm 의존성을 더하지 않습니다.

XDG 설정·데이터·캐시 경로(`XDG_CONFIG_HOME` 등)도 시험 임시 폴더로 돌려, 앱이 개발자의 실제 UI 설정
(`~/.config/app.yangpa.desktop/ui-prefs.json`)을 읽거나 고치지 않습니다. 시험은 임시 설정 파일을 미리 심어 두고 앱이
그 파일을 읽고 다시 쓰는지 확인합니다. 끝나면 단계마다 정한 시간 안에 세션 닫기 → 데몬 끄기(`system.shutdown`) →
`tauri-driver` 끄기를 하고, 그래도 남은 프로세스는 시험 경로(`YANGPA_DATA_DIR`)를 물려받은 것만 골라 기다렸다가 끈 뒤
임시 폴더를 지웁니다(`e2e-window/cleanup.ts`, 그 시험 `cleanup.test.ts`는 도구 없이도 돕니다). 걸린 드라이버나 죽은
데몬이 있어도 훅 제한 시간을 넘지 않아 프로세스·포트가 남지 않습니다.

```sh
sudo apt-get install webkit2gtk-driver xvfb
cargo install tauri-driver --version 2.1.0 --locked
npm run test:window:build          # 프런트엔드, yangpad(CI처럼 따로), webdriver 기능을 켠 yangpa-desktop
xvfb-run -a npm run test:window
```

리눅스가 아니거나 화면(`DISPLAY`·`WAYLAND_DISPLAY`), `tauri-driver`, `WebKitWebDriver`, 빌드한 앱, 그 옆의 `yangpad`가
없으면 무엇이 없는지 적고 건너뜁니다(`YANGPA_WINDOW_REQUIRED=1`이면 실패, CI가 이렇게 돌림). 경로는
`YANGPA_WINDOW_APP`·`TAURI_DRIVER`·`WEBKIT_WEBDRIVER`로 바꿀 수 있습니다.

`webdriver` 기능을 켠 빌드는 `tauri-driver`가 띄울 때(`TAURI_WEBVIEW_AUTOMATION=true`)만 창의 incognito를 끄고 웹뷰 데이터를
`YANGPA_DATA_DIR/webview`(시험 임시 폴더)에 둡니다. WebKit 자동화가 incognito(임시 `WebContext`) 창을 몰지 못하기 때문입니다.
배포 묶음은 이 기능을 켜지 않습니다.

자동화하지 않은 것: macOS(`tauri-driver`가 지원하지 않음)·Windows(Edge WebDriver 연결 안 함)의 실제 창, 통화·화면 공유
(카메라·마이크·화면 선택 대화상자), OS 알림·트레이, 파일 대화상자, 화면 캡처 제외, 서명·공증 경고. 이들은 릴리스 전에
사람이 확인합니다(저장소 CONTRIBUTING.md "릴리스 절차").

## 데몬 연결

- 기본 엔드포인트(`yangpa_rpc::endpoint::default_endpoint`)에 붙고, 소유자 토큰(`<데이터 경로>/owner.token`,
  또는 `YANGPA_TOKEN`)으로 `auth.hello` 후 모든 이벤트를 구독합니다.
- 데몬이 없으면 한 번 띄웁니다. 찾는 순서: `YANGPA_DAEMON_BIN`(주면 이것만) → 앱 실행 파일 옆 `yangpad`(사이드카) →
  옆의 `yangpad-<트리플>` → 디버그 빌드만 `CARGO_TARGET_DIR`·작업 공간 `target`의 `debug`·`release` → `PATH`.
  앱과 분리된 프로세스로 띄우고, 앱이 끝날 때 붙어 있던 데몬에 `system.shutdown`을 보냅니다. 띄울 때 앱 PID를
  `YANGPA_PARENT_PID`로 넘겨, 앱이 강제로 꺼져도 데몬이 앱 프로세스의 끝을 보고 스스로 끝납니다. `YANGPA_NO_SPAWN=1`이면
  띄우지도 끄지도 않습니다(데몬을 따로 관리할 때).
- 끊기면 0.5초부터 최대 10초 간격으로 다시 붙고, 다시 붙으면 프런트엔드가 상태를 전부 새로 읽습니다.
- 로그는 `YANGPA_LOG`(tracing EnvFilter)로 조절합니다. 메시지 내용·키·토큰은 어떤 수준에서도 남기지 않습니다.

## 웹뷰 명령·이벤트

| 이름 | 설명 |
|---|---|
| `rpc_call(method, params?) → 결과` | 데몬 API 호출. 오류는 `{code, message, data?}`(데몬 오류 그대로, 연결 없음은 `-32001`). `auth.hello`·`events.*`는 막음 |
| `daemon_status() → DaemonState` / 이벤트 `yangpa-daemon` | `{state: connecting \| spawning \| connected(version?) \| error(message)}` |
| `daemon_retry()` | 다시 붙기 대기를 건너뜀 |
| 이벤트 `yangpa-events` | 데몬 알림 묶음(`Notification[]`, 약 16ms 단위). 프런트엔드는 화면 프레임마다 한 번 적용 |
| `set_content_protection(enabled) → bool` | 창 캡처 제외. 지원하면 true |
| `set_capture_watch(enabled) → bool` / 이벤트 `yangpa-capture` | 휘발성 채널을 보는 동안 캡처·녹화 감지(윈도, 아래). 이벤트는 `{kind: "screenshot", tool}` 또는 `{kind: "recording", tools}`(빈 목록이면 꺼짐) |
| `open_external(url)` | http(s)만, 사용자 정보 든 주소 거부, 셸 없이 기본 브라우저로 |
| `ui_prefs_get()` / `ui_prefs_set(prefs)` | UI 설정(테마·패널·캡처 차단·마지막 일반 채널·OS 알림 설정·신뢰 경고 건너뛰기) JSON(예전 판이 남긴 저전력 항목은 읽을 때 버림), 설정 폴더 `ui-prefs.json`(64KiB, 0600). 저장하면 알림 설정을 바로 적용 |
| `shell_texts(texts)` | 셸이 직접 띄우는 문구(트레이 메뉴·OS 알림·화면 이름)의 화면 언어 번역. 웹뷰가 시작할 때 보내고(`src/main.tsx`), 트레이 글은 바로 바꿈. 받기 전에는 한국어 |
| `notify_set_viewing(space?, channel?)` / 이벤트 `yangpa-open-channel` | 지금 보고 있는 채널(없으면 둘 다 null)을 알려 그 채널 알림을 거름. 알림을 누르면 창을 앞으로 가져오고 `{space, channel}` 이벤트로 그 채널을 엶 |
| 프로토콜 `yangpa-blob` | `/blob/<space>/<blob>/<key_from>?mime=` → `blob.read` 바이트(아래). `mime=auto`면 앞 바이트로 이미지 형식만 알아봄 |
| `local_preview_open(path) → {token, mime, size}` / `local_preview_close(token)` | 보내기 전 동영상·음성 미리보기(아래). 앞 바이트가 웹뷰에서 재생되는 동영상·음성인 일반 파일만 등록(최대 16개, 10분) |
| 프로토콜 `yangpa-blob` `/local/<token>` | 등록한 로컬 파일을 구간(최대 4MiB, 늘 206)으로. 데몬을 거치지 않음 |

권한(capabilities/default.json)은 메인 창에 위 앱 명령, 이벤트 수신, 파일 열기·저장 대화상자만 줍니다.

## 동영상·음성 첨부의 썸네일·길이

데몬에는 영상 디코더가 없어서, 동영상·음성을 붙이면 앱이 보내기 전에 웹뷰의 `<video>`·`<audio>`로 길이·크기와
장면 하나(길이의 10%, 최대 1초 지점)를 읽어 `message.send`의 `thumbnail_base64`·`width`·`height`·`duration_ms`로 싣습니다
(`src/lib/media.ts`). 썸네일은 긴 변 320px부터 줄여 가며 60KiB 이하 JPEG로 만들고, 데몬이 다시 검사해 JPEG로 다시
만듭니다(MP4·MOV 길이는 데몬이 파일에서 읽은 값이 우선). 웹뷰는 경로를 직접 열 수 없으므로 고른 파일을
`local_preview_open`으로 등록해 무작위 토큰 주소(`yangpa-blob` `/local/<token>`)로 읽고, 다 읽으면 바로 닫습니다.
응답은 블롭 응답과 같은 캐시 금지 헤더에 앱 출처 CORS를 붙여(`crossOrigin="anonymous"`로 캔버스를 오염시키지 않게)
냅니다. 읽지 못하면(코덱·시간 초과 10초) 힌트 없이 보냅니다. 휘발성 채널에서는 미리보기를 열지 않습니다(웹뷰 미디어
경로를 거치지 않게).

## 캐시·저장 위치(휘발성 데이터가 디스크에 남지 않게)

- 창은 `incognito`로 만듭니다. 플랫폼별 동작:
  - **리눅스(WebKitGTK)**: ephemeral `WebContext` — HTTP 캐시·localStorage·IndexedDB·쿠키가 메모리에만 있습니다.
  - **macOS(WKWebView)**: `WKWebsiteDataStore.nonPersistentDataStore` — 위와 같음.
  - **윈도(WebView2)**: InPrivate 모드 — 브라우징 데이터는 메모리에만. 다만 WebView2는 사용자 데이터 폴더
    (`%LOCALAPPDATA%\app.yangpa.desktop\EBWebView`)를 만들고 GPU 셰이더 캐시·크래시 보고 같은 엔진 파일을 쓸 수 있습니다.
    그래서 `--disk-cache-size=1 --media-cache-size=1 --disable-background-networking`도 함께 줍니다.
- 창이 가려지거나 최소화돼도 앱이 느려지지 않게 WebView2의 백그라운드 절전을 끕니다:
  `--disable-background-timer-throttling --disable-renderer-backgrounding --disable-backgrounding-occluded-windows`와
  `--disable-features`의 `IntensiveWakeUpThrottling,CalculateNativeWinOcclusion`. 이 문자열은 wry 기본값을 통째로 바꾸므로
  기본값(`msWebOOUI,msPdfOOUI,msSmartScreenProtection`)도 같은 `--disable-features`에 함께 둡니다. 데몬은 시작할 때 윈도
  전원 조절(EcoQoS, 효율 모드)에서 빠집니다(`yangpa-daemon/src/main.rs`).
- 그래서 UI 설정은 localStorage 대신 `ui_prefs_*` 명령으로 파일에 둡니다(휘발성 채널 ID는 넣지 않음).
- `yangpa-blob` 응답은 모두 `Cache-Control: no-store`, `Pragma: no-cache`, `nosniff`, `CSP: default-src 'none'; sandbox`.
  렌더링 가능한 형식(PNG/JPEG/GIF/WebP/AVIF, MP4/WebM, 음성)만 그 형식으로, 나머지(SVG·HTML 포함)는
  `application/octet-stream`으로 냅니다. 블롭 크기만 기억하고(내용 없음) 동영상 Range 요청은 필요한 구간만 읽습니다.
  형식이 적히지 않은 프로필·크루 아이콘·이모지 이미지는 `?mime=auto`로 불러, 첫 바이트가 png·jpeg·gif·webp·avif일 때만
  그 형식으로 냅니다(전체 응답일 때만, 나머지는 octet-stream). `<img>`는 모두 `decoding="async"`로 메인 스레드 밖에서 풉니다.
- 휘발성 채널의 이미지·영상은 프로토콜 URL을 `<img>`에 바로 쓰지 않고 바이트를 읽어 `blob:` URL로 만들고,
  메시지가 사라지거나 화면에서 빠지면 `URL.revokeObjectURL`로 지웁니다. 휘발성 내용은 localStorage·sessionStorage·
  IndexedDB에 쓰지 않습니다(스토어는 메모리, 채널을 떠나면 비움).

## 화면 캡처 제외

- **윈도**: `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)` — 화면 녹화·캡처 도구에 창이 검게 나옵니다(윈도 10 2004+).
- **macOS**: `NSWindow.sharingType = none` — 스크린숏·화면 공유에서 창이 빠집니다(최신 macOS의 일부 캡처 경로는 무시할 수 있음).
- **리눅스**: 지원하지 않습니다(X11·Wayland에 해당 API 없음). 명령은 false를 돌려주고 화면에 "지원 안 됨"을 표시합니다.
- 어떤 경우든 휴대폰 카메라 촬영·수정된 클라이언트는 막지 못합니다. 휘발성 채널 배너가 이를 그대로 알립니다.

### 캡처·녹화 감지

휘발성 채널을 보는 동안(입장·퇴장 채널은 들어와 있을 때) 스크린샷이나 녹화가 감지되면 채널 화면을 검게 가리고
"해당 채널에서는 스크린샷이나 녹화가 금지되어 있어요."를 띄웁니다(`CaptureShield`). 윈도에는 "지금 찍는 중"을 알려 주는
공개 API가 없어 다음으로 짐작합니다(`src-tauri/src/capture.rs`, 250ms마다, 프로세스 목록은 500ms마다).

- 클립보드에 그림이 새로 들어왔는데 넣은 쪽이 시스템(PrtScn·Alt+PrtScn)이거나 알려진 캡처 도구일 때: 스크린샷.
  다른 앱에서 그림을 복사한 것은 세지 않습니다. 클립보드는 열지 않고 형식과 넣은 창만 봅니다.
- 캡처 도구(Win+Shift+S 영역 캡처·캡처 도구·ShareX·Greenshot·Lightshot·PicPick·알캡처)가 새로 뜰 때: 스크린샷.
  트레이에 늘 떠 있는 것은 세지 않습니다. 확인을 누르면 걷힙니다.
- 녹화 프로그램(OBS·반디캠·오캠·곰캠·Camtasia·XSplit·Xbox Game Bar 녹화)이 켜져 있는 동안: 계속 가리고, 꺼지면 걷힙니다.
- 앱 창에서 누른 PrtScn(keyup): 모든 OS에서 스크린샷으로 봅니다.

가리는 것은 찍힌 뒤의 알림입니다. 찍힌 화면에서 창 내용을 빼는 일은 위의 캡처 제외가 하고, 휴대폰 촬영이나 목록에 없는
프로그램은 감지하지 못합니다. 안드로이드는 나중에 `FLAG_SECURE`와 스크린샷·녹화 콜백(안드로이드 14·15)으로 붙입니다.

## OS 알림

데몬의 `message.new`·`friend.request`·`confirm.pending`을 받아 `notify.rs`가 띄웁니다. 설정은 사용자 설정 → 알림
(`ui-prefs.json`의 `notify`·`notifyCrew`·`notifyPreview`, 기본 켬·모든 메시지·미리보기 켬).

- 메시지: 남이 보낸 것만. 1대1 공간은 모두, 크루는 설정에 따라 모든 메시지(스레드 답글은 멘션일 때만)·멘션만·끔. 내 공간(혼자 쓰는 메모)은 띄우지 않음.
- 창이 포커스를 가진 동안에도 띄우되, 지금 보고 있는 채널(스레드가 아닌 메시지)은 빼고 친구 요청·확인 요청도 빼고(화면에 보임) 띄웁니다.
  윈도는 전경 창(`GetForegroundWindow`)을 직접 봅니다. 웹뷰2 포커스 이벤트는 시작할 때 첫 포커스를 놓쳐, 창을 한 번
  내렸다 올릴 때까지 보고 있는 채널도 알림이 뜰 수 있었습니다. 다른 운영체제는 `WindowEvent::Focused`로 기록합니다.
- 휘발성 메시지와 미리보기를 끈 때는 본문 없이 제목만. 제목은 `작성자`(1대1)나 `작성자 · 크루 #채널`, 본문의 `<@ID>`는 `@이름`.
- 한 번에(이벤트 묶음 하나) 넷 이상 오면 바로, 알림을 띄운 뒤 1.5초 안에 둘 이상 더 오면 1.5초 뒤 "새 알림 N개" 하나(본문
  없음)로 묶습니다. 하나만 더 오면 1.5초 뒤 그대로 띄웁니다. 그사이 그 채널을 열었으면 띄우지 않습니다(스레드 답글은 띄움).
- 휘발성 메시지와 미리보기를 끈 때의 '본문 없음'은 메시지 알림 이야기입니다. 친구 요청·확인 요청 알림은 이름만 담습니다.
- 창 밖일 때 대상 채널이 있는 알림이 오면(묶기 전 기준) 작업 표시줄을 깜빡이고, 창이 포커스를 얻으면 끕니다(`request_user_attention`).
- 알림을 누르면 창을 되살려 앞으로 가져오고 그 채널을 엽니다. 윈도는 알림 하나마다 누름을 기다리는 스레드가 생기고 알림이 사라지면 끝납니다.
- 윈도는 알림에 앱 이름이 나오도록 `HKCU\Software\Classes\AppUserModelId\<식별자>`에 `DisplayName`을 씁니다(설치하지 않은
  휴대용 실행 파일도). `target\debug`·`target\release`에서 돌리면 플러그인이 PowerShell 이름을 쓰므로 건너뜁니다.
- 앱을 닫으면 데몬도 끝나므로 알림도 멈춥니다.

## 웹뷰 보안

- CSP: 스크립트는 앱 자신만, 이미지·미디어는 `data:`/`blob:`/`yangpa-blob:`, iframe은 https(누른 뒤에만 불러오는 플레이어)만.
- 앱 밖으로의 이동은 막고(https iframe은 macOS·리눅스에서 같은 처리기를 거쳐 허용, 윈도는 최상위 창 이동만 처리기에 오므로
  https도 막음), `window.open` 새 창은 모두 거부합니다.
  링크는 `open_external`로 기본 브라우저에서 엽니다.
- 마이크·카메라·화면 공유 권한은 앱 화면 출처(`tauri://localhost`, `http(s)://tauri.localhost`)에만 줍니다
  (플레이어 iframe의 `allow`에는 넣지 않음).
- `yangpa-blob`이 블롭 전체를 메모리로 올리는 읽기는 동시에 4개까지만 합니다(큰 동영상 여러 개가 한꺼번에 보여도 메모리 상한).
- 리눅스 WebKitGTK는 WebRTC·미디어 스트림을 켜고, 웹 콘솔 메시지가 표준 출력으로 나가지 않게 합니다.

## 음성·화면 공유(src/voice)

WebRTC 메시. 시그널링은 `voice.signal`, 참가자·미디어 키는 `voice.participants` 이벤트. 영상 송신기는 미리 만들어 두고
`replaceTrack`으로 켜고 끄며, 화면 공유는 보는 사람이 "보기"를 눌러야 영상을 받습니다. Opus는 `usedtx=1`.
신경망 음성 코덱(Mimi·DAC, 사용자 설정 → 음성·화면공유)은 메시 통화에서 상대마다 데이터 채널(`yangpa-neural`, id 7)로
보내고, 계산은 앱의 Rust 쪽(`src-tauri/src/neural/`, ONNX Runtime을 모델 폴더에서 엶)이 합니다. 모델은 앱에 넣지 않고
배포 빌드가 설치 파일(리소스)과 포터블 zip에 exe 옆 `models` 폴더로 넣습니다(`src-tauri/models`에 받아 둠). 앱은 열기 전에 파일을
`scripts/neural-models.txt`의 sha256과 맞춰 봅니다(목록에 없는 윈도 밖 런타임 라이브러리는 확인하지 않음). 개발 중에는 `YANGPA_MODELS_DIR`로 모델 폴더를 가리킬 수 있고,
그 값이 있으면 `cargo test -p yangpa-desktop neural`이 실제 모델로 왕복 시험도 합니다.
프레임 암호화는 `createEncodedStreams`(연결을 `encodedInsertableStreams`로 만듦, 없으면 `RTCRtpScriptTransform`) 워커에서
AES-256-GCM. 변환은 트랜시버를 만들자마자 달고 마이크·화면 트랙은 그 뒤에 넣습니다(크로뮴이 늦게 단 변환을 무시하는 문제):
`[평문 코덱 헤더 | 암호문+태그 | IV(12) 세대(1) 버전(1)]`. IV는 송신 워커마다 무작위 8바이트 + 4바이트 카운터라
같은 세대 키를 쓰는 참가자끼리 nonce가 겹치지 않습니다. 협상 신호는 보내고 받는 쪽 모두 순서대로 처리하고, 원격 설명보다
먼저 온 ICE 후보는 모아 두었다가 넣습니다. 보통 채널은 모든 연결이 Tor로 오가고 Tor는 UDP를 나르지 못하므로 통화를
시작하지 않습니다("이 채널의 통화는 아직 Tor로 나르지 못합니다"). 이 기기가 참가한 P2P 전용 채널은 IP가 보이는 것을
받아들인 곳이라 직접 연결로 통화합니다(참가 전에는 통화도 잠김, `voiceBlockReason`). `voice.join` 결과의 `ice_servers`는
내 STUN 서버와 쓰기로 한 크루 STUN 서버뿐입니다. 웹뷰가 프레임 암호화(Encoded Transform)를 지원하지 않으면 전송 암호화만으로
통화하지 않고, 이유를 알리며 참가하지 않습니다.

## 모더레이션 화면

- **메시지 메뉴**: 크루에서 남의 메시지에 '신고하기'(사유 512자, '이 기기에서 숨기기' 기본 켜짐, '대신 차단하기')와
  '이 사람 차단'/'차단 풀기'. 차단한 사람의 메시지는 한 줄로 접히고 '보기'로 그 자리에서만 펼칩니다.
- **크루 설정 → 모더레이션**: '모더레이션의 한계' 안내(서버가 없어 할 수 없는 것), 운영진(추방·밴·메시지 관리 권한)에게는
  신고 목록(처리 전/처리함, `report.new`가 오면 다시 읽고 탭에 새 신고 수 표시)과 밴 목록. 신고 줄에서 '메시지 보기'는 그 메시지로
  건너뛰고, 기각은 바로, 메시지 삭제·추방·밴은 같은 줄에서 한 번 더 확인합니다. 처리 메모는 처리 결과에만 남고(운영진끼리만)
  추방·밴 사유로는 싣지 않습니다. 추방·밴 버튼은 권한과 함께 순위도 봅니다: 이 기기에 불러온 사본으로 본 작성자가 나보다 높거나
  같으면 막고, 사본이 없으면 막지 않고 데몬이 자기 사본으로 확인한다고 알립니다.
- **답장 머리글**: 차단한 사람의 메시지에 남이 단 답장은 원문 발췌 대신 '차단한 사용자의 메시지'로 보입니다.
- **멤버·프로필**: 나보다 순위가 낮은 멤버에게만 추방·밴(사유, 최근 24시간 메시지 지우기). 역할은 내 순위보다 낮은 것만 고치고
  내가 가진 권한만 줄 수 있습니다(데몬 규칙과 같음).
- **설정 → 차단한 사용자**: 이 기기의 차단 목록과 차단 풀기. 차단은 상대에게 알리지 않고, 같은 크루의 다른 사람에게는 그대로 보입니다.
- 데몬에 모더레이션 메서드가 없으면(`-32601`) 해당 화면은 "연결된 데몬이 … 지원하지 않습니다"로 바뀌고 나머지는 그대로 동작합니다.

## 그 밖의 동작

- **설정 → 네트워크·Tor**: Tor 상태와 이 기기의 onion 주소, 그리고 STUN 서버 목록(`net.stun_servers`, 추가·빼기·기본
  목록으로 되돌리기는 바로 `net.set_stun_servers`로 저장). STUN은 P2P 연결(신뢰한 상대·P2P 전용 채널)이 공유기 밖 주소를
  알아낼 때만 쓰고 Tor 연결에는 쓰지 않습니다. 앱은 어디에도 IP 주소나 IP multiaddr를 보이지 않습니다(수신·릴레이 주소 줄
  없음). 저전력·상시 노드·IP 숨기기 설정은 없어졌습니다.
- **신뢰 스위치**: 상대 프로필의 '연결'(사람, `trust.get`·`trust.set_user`)과 크루 설정 → 개요의 '이 크루 신뢰'
  (`SpaceView.trusted`·`trust.set_space`). 신뢰는 그 사람·크루 멤버에게 이 기기의 IP를 보여도 된다는 이 기기의 동의이고, 서로
  신뢰하면 P2P, 한쪽만이면 단방향으로 잇습니다. 켤 때마다 IP 노출 경고를 연 창 위에 겹쳐 띄우고(아래 창의 입력은 그대로, Esc는
  맨 위 창만 닫음) 확인해야만 보냅니다. 끄기는 바로 보냅니다. 경고의 '다시 보지 않기'는 `ui-prefs.json`의 `skipTrustWarning`으로
  남고 설정 → 네트워크·Tor에서 되돌립니다. 프로필은 열린 동안 5초마다 연결 방식을 다시 읽어 말로만 보이고(IP 주소는 어디에도
  없음), 크루 신뢰로 이미 IP가 보이면 그 크루 이름을 알립니다. 함께 쓰는 공간이 없는 사람(만료된 연락처뿐)에게는 구역을 숨기되
  이미 켜 둔 신뢰는 끌 수 있게 남깁니다. 신뢰 스위치를 모르는 데몬이면 두 곳 모두 보이지 않습니다.
- **휘발성 채널의 AI·자동화 읽기**: 휘발성 안내 띠에서 이 기기의 API 토큰이 그 채널을 읽게 할지(`channel.allow_api_read`) 정합니다.
  켤 때는 경고를 확인받고, 켜져 있으면 채널 상단에 'AI 읽기 허용' 표시가 붙습니다(누르면 바로 다시 막음). 기기마다의 설정이라
  채널 관리 권한과 상관없이 참가자 누구나 자기 기기에서 정합니다.
- **폐기된 기기**: 이 기기가 다른 기기에서 폐기되면(`device.wiped`) 화면 메모리를 비우고 온보딩으로 돌아갑니다.
- **첨부 진행률**: `blob.status`(받은 조각·시더 수)는 받기를 눌렀거나 받는 중(진행 알림)일 때만 읽습니다(그려질 때마다 읽으면
  데몬이 연결된 멤버 기기마다 보유 여부를 물음). 받는 중에는 3초마다 다시 읽어 막대로 보이고, 가진 온라인 기기가 없으면 알립니다.
  '저장'은 `blob.save`(대화상자로 고른 경로, 덮어쓰지 않음).
- **저장 공간·보관**: 사용자 설정 → 저장 공간이 `blob.cache_status`(사용량 막대·디스크 여유)·`blob.cache_set_limit`(GiB로 받아
  바이트로 보냄)·`blob.cache_clear`(확인 뒤)·`blob.pins`(보관 목록, 풀기)를 씁니다. 받은 첨부의 핀 단추(보관)는 `blob.pin`·
  `blob.unpin`이고, 보관 여부는 `blob.pins`를 한 번 읽어 메모리에 둡니다(`hooks/pins.ts`. `blob.status`는 멤버 기기마다 물으므로
  쓰지 않음). 내가 올린 첨부와 휘발성 첨부에는 핀이 없습니다. 미리 받기(`seed.set`의 `prefetch`)는 동기화 탭의 시더 칸에 있고,
  이 기능들을 모르는 데몬이면 숨깁니다.
- **확정 기기**: 크루 설정 → 개요에 `crew.committer_candidates`를 보이고, 소유자만 다른 기기를 골라 확인 창을 거쳐
  `crew.set_committer`를 부릅니다. `space.updated`로 `committer`가 바뀌면 다시 읽습니다.
- **긴 채널**: 메시지 목록은 가상 스크롤(1만 개 이상), 데몬 알림은 화면 프레임마다 한 번 묶어 적용합니다.
