# 직접 등록한 CI 러너

GitHub 서버 러너를 쓸 수 없을 때(비공개 저장소의 Actions 사용 한도·결제 문제 등) CI를 직접 등록한 PC에서 돌립니다.

## 켜고 끄기

- 저장소 변수 `CI_RUNNERS`를 `self-hosted`로 두면 `ci.yml`의 작업(Rust·데스크톱 앱, 둘 다 Windows)이
  `self-hosted, Windows, X64, yangpa` 라벨을 가진 러너로 갑니다. 변수를 지우면 다시 GitHub 서버 러너(`windows-latest`)를 씁니다.
- 바꾸는 곳: 저장소 Settings → Secrets and variables → Actions → Variables, 또는
  `gh variable set CI_RUNNERS --body self-hosted` / `gh variable delete CI_RUNNERS`.

라벨에 맞는 러너가 없으면 그 작업은 실패하지 않고 대기열에 남습니다(24시간 뒤 취소). 성능(`bench.yml`)·Tor 누출(`tor-leak.yml`)·
릴리스(`release.yml`)는 리눅스 도구(valgrind·tc·tcpdump·Docker)를 써서 그대로 GitHub 서버 러너를 씁니다.

## Windows 러너 준비

1. 필요한 도구: Git for Windows(`bash`가 PATH에 있어야 함), rustup, Visual Studio C++ 빌드 도구(MSVC·Windows SDK),
   Strawberry Perl(SQLCipher가 쓰는 OpenSSL을 MSVC로 빌드할 때 필요), Python 3(설치 훅 검사), Node(작업이 22를 받아 씀).
   러너가 `shell: bash`를 WSL 실행기(`C:\Windows\System32\bash.exe`)가 아니라 Git의 bash로 찾도록, 러너 프로세스를
   `C:\Program Files\Git\bin`이 PATH 맨 앞에 오게 띄웁니다. 러너 폴더의 `.path`만으로는 셸을 찾는 데 쓰이지 않으므로
   `start-runner.cmd`(`set "PATH=C:\Program Files\Git\bin;%PATH%"` 뒤 `run.cmd`)로 띄웁니다. `Git\usr\bin`은 MSYS perl이
   있어 넣지 않습니다. Strawberry Perl은 `C:\Strawberry`에 설치합니다(`ci.yml`의
   `OPENSSL_SRC_PERL`이 이 경로를 가리킴).
   러너를 평소 쓰는 사용자 계정으로 띄우면 `start-runner.cmd`에서 `CARGO_HOME`·`RUSTUP_HOME`을 러너 전용 폴더(예:
   `actions-runner\_tools\cargo`, `actions-runner\_tools\rustup`)로 지정합니다. 그러지 않으면 CI의
   `dtolnay/rust-toolchain`이 사용자의 stable 툴체인을 올리고, `Swatinem/rust-cache`가 저장 전 정리로 사용자의
   `~\.cargo\bin`(rustup·cargo 바로가기)을 지웁니다(2026-10-21 실제로 일어남). `ci.yml`은 `CI_RUNNERS=self-hosted`일 때
   `rust-cache`를 쓰지 않지만, 툴체인을 나눠 쓰는 문제는 이 분리로만 막힙니다.
   PC 러너에서는 빌드 폴더를 러너 도구 폴더(`actions-runner\_work\_tool\yangpa-target`, `checkout`이 지우지 않음)에 두고
   run마다 이어 써서 바뀐 크레이트만 다시 빌드합니다. `rustc -V`가 바뀌면 그 폴더를 비웁니다. 디스크가 부족하면 러너가
   쉬는 동안 그 폴더를 지워도 됩니다(다음 run이 처음부터 빌드).
2. [actions/runner](https://github.com/actions/runner/releases)의 win-x64 묶음을 러너 폴더(예: 저장소 체크아웃의 `actions-runner\`, `.git/info/exclude`에 추가)에 풀고 SHA-256을 확인합니다.
3. 저장소 Settings → Actions → Runners → New self-hosted runner의 토큰으로 등록합니다.
   Rust·Tauri 빌드가 경로 길이 한도(260자)에 걸리지 않게 Windows 긴 경로(`LongPathsEnabled`)를 켜 두거나 작업 경로를 짧게 둡니다.

   ```bat
   config.cmd --url https://github.com/dhtpdud/YANGPA-Dev --token <토큰> --labels yangpa --work _work
   ```
4. 서비스로 등록하거나(`--runasservice`, 관리자 권한) 로그온 때 `start-runner.cmd`를 띄우는 예약 작업을 만듭니다.
   CI가 도는 동안 PC가 켜져 있고 로그인돼 있어야 합니다. 다시 띄울 때는 프로세스를 강제로 끄지 말고 예약 작업을 멈췄다
   켭니다(강제로 끄면 GitHub에 남은 세션 때문에 몇 분 동안 작업을 받지 못함).

## 주의

- 러너는 PR의 코드를 그 PC에서 그대로 실행합니다. 저장소를 공개로 바꾸면 남의 포크 PR도 돌 수 있으므로,
  공개로 바꾸기 전에 러너를 먼저 지우세요. 새 공개 저장소에도 러너를 등록하거나 `CI_RUNNERS`를 설정하지 않습니다.
  공개 저장소는 GitHub 서버 러너를 무료로 씁니다.
- 워크플로의 액션은 커밋 해시로 고정합니다(태그가 옮겨져도 바뀌지 않게). 올릴 때는 새 판의 해시와 판 이름 주석을 함께 바꿉니다.
- 등록 토큰은 한 시간 동안만 쓸 수 있고, 저장소나 로그에 남기지 않습니다.
