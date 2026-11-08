# Android 앱

데스크톱 앱(Tauri 2)을 그대로 Android로 빌드합니다. 화면은 같은 React 코드이고 좁은 화면에서는 칸을 하나씩 보여 줍니다.
기획서의 UniFFI 바인딩 대신 이 길을 택한 까닭은 [decisions.md](decisions.md) "Android 앱"에 있습니다.

## 구조

| 부분 | 데스크톱 | Android |
|---|---|---|
| 데몬 | `yangpad` 사이드카 프로세스 | 앱 프로세스의 전용 스레드(`src-tauri/src/embedded.rs`, `yangpa_daemon::host::Host`) |
| 로컬 API | 유닉스 소켓·named pipe | 같은 유닉스 소켓(앱 폴더 안 `daemon/yangpad.sock`). `daemon.rs`와 프런트엔드는 그대로 |
| 백그라운드 | 트레이 | 포그라운드 서비스(`YangpaService.kt`). 프로세스를 살려 두기만 합니다 |
| 메시지 알림 | 알림 플러그인 | `YangpaNotifier.kt`를 JNI로 바로 부름. 플러그인 호출은 화면이 없으면 패닉합니다 |
| DB 키 | OS 키 저장소 | Android Keystore(`android-native-keyring-store`) |
| 파일 고르기·저장 | 경로 | `content://`를 앱 캐시 경로와 이음(`file_import`·`file_export`, `client.ts`의 `androidTransport`) |
| 휘발성 채널 캡처 차단 | 창 캡처 제외 | `FLAG_SECURE`(스크린샷·녹화·최근 앱 미리보기를 막음). 같은 때 키보드 학습 끄기 |
| 브리지 | exe 옆 `pt/lyrebird` | 네이티브 라이브러리 `liblyrebird.so`(앱 데이터 폴더의 파일은 실행할 수 없음) |

- 사이드카를 쓰지 않는 까닭: Android에는 `externalBin`이 없고, 띄운 자식 프로세스는 Android 12의 유령 프로세스 정리에 걸립니다.
- 오프라인 모드는 Arti를 같은 프로세스에서 다시 만들 수 없어 앱 프로세스를 새로 띄웁니다(`YangpaPlugin.restart`). 화면이 없을 때
  기간이 끝나면 앱이 꺼지고, 다음에 열 때 다시 뜹니다.
- 최근 앱에서 밀어 닫으면 화면만 없어지고 프로세스와 데몬은 남아 메시지·알림을 받습니다(`RunEvent::ExitRequested`를 막음). Tauri
  플러그인이 첫 화면에 묶여 있어 같은 프로세스에서 화면을 새로 쓸 수 없으므로, 다시 열면 프로세스를 새로 띄웁니다(`MainActivity`).
  화면이 없는 동안 `run_mobile_plugin`을 부르면 wry가 패닉하니, 백그라운드 코드에서는 쓰지 마세요(`app.path()`도 같습니다).
- 권한 요청 콜백(`on_permission_request`)은 UI 스레드에서 돕니다. 그 안에서 `webview.url()`을 부르면 시간이 넘어 앱이 죽습니다.
- 웹뷰는 이동 처리기가 최상위 이동만 막습니다(`platform.rs`의 `navigation_allowed`, 윈도와 같은 정책). 권한 요청 콜백은 URL을 물을 수 없어
  `on_page_load`가 기록한 최상위 출처(`ON_APP_PAGE`)로 앱 화면인지 판단합니다. iframe은 CSP의 `frame-src`가 막습니다.
- 앱은 Tor로만 나가므로 웹뷰가 따로 나가는 길을 막습니다. 입력 칸이 보이면 웹뷰가 `content-autofill.googleapis.com`에 양식 해시를 물어 폰의 실제
  IP가 구글에 드러납니다. 이를 끄는 API가 없어 그 주소만 없는 프록시로 보냅니다(`MainActivity.onWebViewCreate`, `ProxyController`).
  자동 완성 서비스(삼성 패스 등)로 입력 칸이 가는 것은 `importantForAutofill`로 막히지 않아 `getSystemService("autofill")`이 `null`을 돌려줍니다.
  그래서 비밀번호 관리자의 자동 입력은 앱 안에서 뜨지 않고, 붙여 넣기는 됩니다. 세이프 브라우징과 웹뷰 지표도 매니페스트에서 끕니다.
- 휘발성 채널을 보는 동안(`FLAG_SECURE`와 같은 때, `YangpaPlugin.setSecure`) 키보드에 `IME_FLAG_NO_PERSONALIZED_LEARNING`을 알립니다.
  웹뷰 클래스 `RustWebView`는 wry가 빌드 때마다 새로 만들므로, 저장소 루트 `.cargo/config.toml`의 `WRY_RUSTWEBVIEW_CLASS_EXTENSION`으로
  `onCreateInputConnection`을 끼웁니다(Tauri CLI는 이 변수를 쓰지 않음). 빌드 뒤 `gen/android/app/src/main/java/app/yangpa/android/generated/RustWebView.kt`에
  `IME_FLAG_NO_PERSONALIZED_LEARNING`이 없으면 `cargo clean -p wry` 뒤 다시 빌드합니다. 학습을 실제로 멈추는지는 키보드 앱 몫입니다.
  휘발성 입력 칸은 맞춤법 검사(키보드 앱의 검사 서비스로 글이 감)도 끄고, 자동 고침은 그대로 둡니다. 맞춤법 검사를 끈 칸에서 추천 단어 줄이
  바뀌는지는 실기기에서 확인하지 않았습니다.
- 고른 파일은 `cacheDir/imports`에 복사되어 데몬이 읽습니다. 메시지를 보내면 지우고(`file_release`), 보내지 않고 앱이 꺼지면 다음 시작 때 비웁니다.
- 서버 푸시가 없어 프로세스가 죽으면 메시지를 받지 못합니다. 데몬을 띄우는 Rust 쪽은 화면이 처음 만들어질 때 뜨므로, 재부팅이나
  프로세스 종료 뒤에는 앱을 한 번 열어야 합니다.

코틀린 코드는 `apps/desktop/src-tauri/gen/android/app/src/main/java/app/yangpa/android/`에 있고 `gen/android`는 저장소에
넣습니다(서비스·권한을 손으로 고쳤으므로 `tauri android init`을 다시 돌리지 않습니다). 패키지 이름 `app.yangpa.android`는
`tauri.android.conf.json`에서 정합니다. Play에 처음 올린 뒤에는 바꿀 수 없습니다.

## 빠진 기능

| 기능 | 까닭 |
|---|---|
| 화면공유 보내기·화면 소리 | ffmpeg·WASAPI에 묶여 있고 Android WebView에 `getDisplayMedia`가 없음. 받아서 보기는 됨 |
| 신경망 음성 코덱 | 모델이 큼. 통화는 Opus로 함 |
| 데이터 폴더 바꾸기 | 앱 전용 폴더만 씀 |
| 트레이·자동 시작 | Android에 없는 개념(자동 시작은 위의 재부팅 제약) |

## 실제 기기 시험

2026-11-04 갤럭시 Z 플립5(Android 16, WebView 156)에서 디버그 빌드로 확인했습니다. 릴리스 빌드는 코드 축소(R8)가 있어 따로 시험합니다:
Rust가 JNI로 부르는 `YangpaNotifier`처럼 코틀린에서 아무도 부르지 않는 코드는 `proguard-rules.pro`에 유지 규칙을 적어야 지워지지 않습니다.

- 시작: 데몬, Android Keystore의 DB 키(프로세스를 새로 띄워도 암호화 DB가 열림), 포그라운드 서비스, Tor 부트스트랩.
- PC 노드와 Tor로 양방향 메시지, 파일 보내기, 저장 창으로 저장.
- 화면이 꺼진 상태와 최근 앱에서 밀어 닫은 상태의 메시지 수신·알림, 알림을 눌러 채널 열기.
- 앱을 밀어 닫고 깊은 절전(`dumpsys deviceidle force-idle`, deep=IDLE)을 건 상태의 메시지 수신·알림. 포그라운드 서비스 프로세스는
  절전 중에도 네트워크가 막히지 않습니다. USB가 꽂혀 있어 CPU가 완전히 잠드는 상태는 시험하지 못했습니다.
- 휘발성 채널: 받은 글·이미지, 입력 중인 글, 보낸 글에 표식을 넣고 앱 폴더 전체와 logcat을 찾아 흔적이 없음을 확인. 받은 이미지는 `blob:`
  주소로만 표시되고 웹뷰 HTTP 캐시에 들어가지 않습니다.
- 뒤로 가기(메뉴·창·칸·앱 내리기), 길게 눌러 메뉴, 키보드, 초대 링크.
- 휘발성 채널의 `FLAG_SECURE`(스크린샷이 검게 나옴), 나가면 풀림.
- 혼자 있는 Tor 통화의 마이크 권한·녹음, 앱을 내려도 녹음 유지, 끝나면 서비스 종류 복귀.
- Snowflake를 켠 채 재시작해 Tor 연결과 메시지 수신(`liblyrebird.so` 실행 확인). 트래픽 경로를 패킷으로 보지는 않았습니다.
- 오프라인 모드 들어가기·나오기(프로세스 재시작), 계정 삭제.
- 앱 UID의 TCP·UDP 상대를 `/proc/net`에서 시작부터 채널을 열 때까지 모아 Tor 연결만 있음을 확인(고치기 전에는 구글 QUIC 연결이 보임).
  입력 칸을 눌러도 `dumpsys autofill`에 세션이 생기지 않음.
- `location.assign`으로 외부 주소 이동이 막힘, 앱 화면의 마이크는 됨. YouTube 플레이어 iframe은 뜨지만 재생까지는 보지 않았습니다.
- 고른 파일의 복사본이 보내자마자 지워지고, 지운 뒤 PC가 Tor로 받은 파일의 해시가 같음.
- 기기 연결 QR 찍기(2026-11-07, WebView 157): 실제 카메라는 824×1920으로 열리고, PC 화면의 QR이 보인 뒤 0.26초 만에 읽어 번호까지 감.
  카메라가 켜지는 데는 2.4초가 걸립니다. 운영 CSP에서 wasm 컴파일 11ms, 1080×1080 프레임 해독 약 85ms.

릴리스 빌드(R8 포함)에서는 시작, 서비스, Tor로 PC와 연결, 밀어 닫은 뒤의 알림, 앱을 새로 띄우는 채널 링크, 그 링크로 연 휘발성 채널의 캡처 차단을
확인했습니다. 파일 고르기와 통화는 디버그 빌드에서만 시험했습니다.

키보드 학습 끄기(2026-11-07)는 실기기에서 아직 확인하지 않았습니다.

두 사람의 실제 음성 통화, 통화의 카메라, 배터리 소모, 장시간 대기, 재부팅, 다른 제조사 기기는 아직 시험하지 않았습니다. 시험한 폰의 Avast One은
디버그 APK를 "의심스러운 앱"으로 표시했습니다. Play 서명 빌드에서도 뜨는지 확인이 필요합니다.

## 빌드

SQLCipher의 OpenSSL을 Unix식 `make`·`perl`로 빌드해서 Windows에서는 NDK로 바로 빌드하지 못합니다. Docker Desktop은 이 PC의
Windows 빌드에서 AF_UNIX 소켓 파일을 지우지 못해 시작이 실패하므로, 전용 WSL 배포판을 씁니다.

```bash
wsl --import yangpa-android C:\Users\<사용자>\WSL\yangpa-android <우분투 rootfs>.tar.gz --version 2
wsl -d yangpa-android -u root --exec bash scripts/android-toolchain.sh
```

`android-toolchain.sh`는 Rust, Node, JDK, Android SDK·NDK를 설치하고 SDK 라이선스에 동의합니다. 빌드는 Windows 폴더를 바로 쓰지
말고 WSL 안으로 복사해서 돌립니다(`/mnt/c`는 느림).

```bash
wsl -d yangpa-android -u root --exec bash -lc "rsync -a --delete --exclude target --exclude node_modules --exclude .git --exclude build --exclude .gradle --exclude jniLibs /mnt/c/<저장소>/ /root/yangpa/ && cd /root/yangpa && scripts/build-android.sh"
```

```bash
scripts/build-android.sh            # 시험용 APK(app-universal-debug.apk)
scripts/build-android.sh --release  # Play용 AAB
```

릴리스를 폰에서 시험하려면 `npx tauri android build --apk --target aarch64`로 만든 APK를 `zipalign -P 16`한 뒤 디버그 키로 서명해
설치합니다(같은 키면 디버그 빌드 위에 데이터를 유지한 채 깔립니다). 릴리스에서는 웹뷰 디버깅과 `run-as`가 되지 않습니다.

폰에는 Windows 쪽 SDK의 `adb`로 설치합니다: `adb install -r app-universal-debug.apk`. 로그는 `adb logcat -s RustStdoutStderr`.

### 서명

Play용 AAB에는 업로드 키가 필요합니다. 키는 저장소에 넣지 않습니다(`gen/android/.gitignore`).

```bash
keytool -genkey -v -keystore yangpa-upload.jks -keyalg RSA -keysize 4096 -validity 10000 -alias upload
```

`gen/android/key.properties`에 `storeFile`, `storePassword`, `keyAlias`, `keyPassword`를 적습니다.

## Google Play

요구 사항은 2026-11 기준입니다. 올리기 전에 [Play Console 도움말](https://support.google.com/googleplay/android-developer)을 다시 보세요.

코드에 반영한 것:

- 라이선스: `LICENSE`, `NOTICE.md`, `LICENSE-SUPPORTER.md`, `TRADEMARK.md`, lyrebird 라이선스를 앱 `assets/licenses`에 넣습니다.
- `targetSdk` 36(2026-08-31부터 필수), AAB, arm64-v8a. `liblyrebird.so`를 포함한 모든 `.so`가 16KB 페이지로 정렬됩니다(NDK r28 이상).
- 포그라운드 서비스 종류 `specialUse`(메시지 대기), 통화 중에는 `microphone`을 더합니다. `dataSync`는 Android 15부터 하루 6시간 제한이 있어 쓰지 않습니다.
- 앱 안 계정 삭제(`identity.delete`, 설정 › 기기).
- 결제: Patreon·GitHub Sponsors로 안내하는 단추와 금액 문구를 Android에서 뺐습니다. 다른 곳에서 결제한 혜택을 로그인해 쓰는 것은
  Play가 "소비 전용 앱"으로 허용합니다. 앱 안에서 다른 결제로 이끌면 안 되므로 이 문구를 되살리지 마세요.
- `allowBackup=false`와 `data_extraction_rules.xml`: DB 키와 메시지가 Google 백업으로 나가지 않습니다.
- 웹뷰의 구글 자동 완성 조회·세이프 브라우징·지표를 끕니다(위 "구조").

Play Console에서 직접 할 일:

- 개발자 계정. 개인 계정은 프로덕션 출시 전에 비공개 테스트 조건이 있습니다.
- 앱 서명(Play App Signing)과 업로드 키.
- 개인정보처리방침 URL, 데이터 보안 양식(개발자 서버가 없음, Tor·P2P 통신).
- 포그라운드 서비스 신고: `specialUse`(사유는 `AndroidManifest.xml`의 `PROPERTY_SPECIAL_USE_FGS_SUBTYPE`), `microphone`.
- 소셜 앱의 아동 안전 표준: 공개 정책 페이지와 연락 담당자.
- 앱 밖 계정 삭제 안내 URL.
