#!/usr/bin/env bash
# Windows 배포 빌드(설치 파일 NSIS·MSI + 포터블 zip)를 한 번에 만듭니다. Git bash에서 저장소 루트 기준으로 돌립니다.
#
#   scripts/build-windows-release.sh            설치 파일(nsis,msi)과 포터블 zip
#   BUNDLES=nsis scripts/build-windows-release.sh   MSI를 건너뜀(WiX 단계만큼 빠름)
#   NO_PORTABLE=1 …                              포터블을 만들지 않음
#   NO_MODELS=1 …                                신경망 음성 코덱 모델(exe 옆 models 폴더)을 설치 파일·포터블에 넣지 않음
#   NO_FFMPEG=1 …                                화면공유 인코더 ffmpeg(exe 옆 ffmpeg 폴더)를 설치 파일·포터블에 넣지 않음
#   NO_LYREBIRD=1 …                              Tor 브리지 전송 lyrebird(exe 옆 pt 폴더)를 설치 파일·포터블에 넣지 않음
#   DIST_DIR='D:\yangpa-build' …                   배포 빌드 폴더(기본 C:\yangpa-build, 아래 "빌드 머신 경로" 참고)
#
# 배포 빌드 폴더를 지우지 않고 이어 쓰므로, 바뀐 크레이트와 마지막 링크(LTO)만 다시 합니다. 데몬·CLI는 데스크톱
# 앱과 따로 빌드합니다: 한 번에 빌드하면 기능 통합으로 데몬 쪽 serde_json에 `unbounded_depth`(Tauri가 켬)가 들어가
# 데몬 동작이 바뀝니다. 설치본·포터블에 들어가지 않는 yangpa-mcp는 빌드하지 않습니다.

set -euo pipefail

TARGET=x86_64-pc-windows-msvc
BUNDLES="${BUNDLES:-nsis,msi}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

START=$SECONDS
log() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }
STEP=""
# 단계마다 걸린 시간을 남겨, 다음에 어디가 느린지 바로 보이게 합니다.
step() {
  if [[ -n "$STEP" ]]; then log "$STEP 끝 ($((SECONDS - step_start))초)"; fi
  STEP="$1"
  step_start=$SECONDS
  if [[ -n "$STEP" ]]; then log "$STEP 시작"; fi
}
fail() { echo "오류: $*" >&2; exit 1; }

case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*) ;;
  *) fail "Windows(Git bash)에서만 돌립니다" ;;
esac

# SQLCipher가 쓰는 OpenSSL(openssl-src)은 MSVC 빌드에 Windows용 perl이 필요합니다(Git의 MSYS perl은 안 됨).
if [[ -z "${OPENSSL_SRC_PERL:-}" && -x /c/Strawberry/perl/bin/perl.exe ]]; then
  export OPENSSL_SRC_PERL='C:\Strawberry\perl\bin\perl.exe'
fi

# 빌드 머신 경로: 배포 바이너리에 사용자 이름·로컬 폴더가 남지 않게 합니다. 릴리스는 기호·디버그 정보를 빼지만
# (Cargo.toml strip), 소스 경로는 문자열로 남습니다. Rust 코드의 경로는 --remap-path-prefix로 바꿀 수 있지만, 데몬에
# 함께 들어가는 C 라이브러리(aws-lc의 __FILE__, SQLCipher용 OpenSSL의 설치 경로 상수)는 cc·cmake·nmake로 빌드돼 그
# 옵션이 닿지 않습니다(87d6eec 빌드의 yangpad.exe에 C:\Users\<이름>\… 95건). 그래서 배포 빌드는 cargo 레지스트리
# (CARGO_HOME)와 빌드 폴더(CARGO_TARGET_DIR)를 사용자 이름이 없는 DIST_DIR 아래에 둡니다. 개발용 ~/.cargo·target/은
# 건드리지 않습니다. 처음 한 번은 레지스트리를 새로 받고 전체를 빌드하고, 그 뒤로는 그 폴더를 이어 써 증분입니다.
DIST_DIR="${DIST_DIR:-C:\\yangpa-build}"
user_name="${USERNAME:-}"
if [[ -n "$user_name" && "${DIST_DIR,,}" == *"${user_name,,}"* ]]; then
  fail "DIST_DIR($DIST_DIR)에 사용자 이름이 들어 있어 배포 바이너리에 남습니다"
fi
dist="$(cygpath -u "$DIST_DIR")"
mkdir -p "$dist/cargo" "$dist/target"
# 개발용 cargo 설정이 있으면 그대로 씁니다. rustup 툴체인 위치(RUSTUP_HOME)는 바꾸지 않습니다.
dev_cargo_home="${CARGO_HOME:-${USERPROFILE:-}\\.cargo}"
for c in config.toml config; do
  src="$(cygpath -u "$dev_cargo_home")/$c"
  if [[ -f "$src" ]]; then cp -p "$src" "$dist/cargo/$c"; fi
done
if [[ -z "${RUSTUP_HOME:-}" && -n "${USERPROFILE:-}" ]]; then
  export RUSTUP_HOME="${USERPROFILE}\\.rustup"
fi
CARGO_HOME="$(cygpath -w "$dist/cargo")"
CARGO_TARGET_DIR="$(cygpath -w "$dist/target")"
export CARGO_HOME CARGO_TARGET_DIR
# Rust 코드 경로는 짧은 토큰으로 바꿉니다(뒤에 쓴 규칙이 우선하므로 구체적인 것을 뒤에). 맞는 접두사가 없으면 무해합니다.
remap=()
[[ -n "${USERPROFILE:-}" ]] && remap+=( "--remap-path-prefix=${USERPROFILE}=~" )
remap+=( "--remap-path-prefix=${CARGO_TARGET_DIR}=target" "--remap-path-prefix=${CARGO_HOME}=cargo" )
export RUSTFLAGS="${remap[*]}${RUSTFLAGS:+ $RUSTFLAGS}"

if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
  log "주의: 커밋하지 않은 변경이 있습니다. 그 상태 그대로 빌드합니다"
fi
SHA="$(git rev-parse --short=7 HEAD)"
VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version *= *"\(.*\)"/\1/p' Cargo.toml)"
[[ -n "$VERSION" ]] || fail "Cargo.toml에서 작업 공간 판을 읽지 못했습니다"
OUT="$dist/target/$TARGET/release"
log "YANGPA $VERSION ($SHA) 빌드, 묶음: $BUNDLES, 빌드 폴더: $DIST_DIR"

step "데몬·CLI 릴리스 빌드"
cargo build --release --locked --target "$TARGET" -p yangpa-daemon -p yangpa-cli --bin yangpad --bin yangpa

step "사이드카 넣기"
mkdir -p apps/desktop/src-tauri/binaries
cp "$OUT/yangpad.exe" "apps/desktop/src-tauri/binaries/yangpad-$TARGET.exe"

step "프런트엔드 패키지"
# package-lock.json이 그대로면 npm ci(node_modules를 지우고 다시 받음)를 건너뜁니다.
stamp=apps/desktop/node_modules/.package-lock.json
if [[ -f "$stamp" && ! apps/desktop/package-lock.json -nt "$stamp" ]]; then
  log "node_modules가 최신이라 건너뜀"
else
  (cd apps/desktop && npm ci --prefer-offline --no-audit --no-fund)
fi

# 목록(<폴더 안 경로> <sha256> <주소> [zip 안 경로])의 파일을 받아 단계 폴더에 놓고 sha256을 맞춰 봅니다. 받은 파일은
# 배포 빌드 폴더(캐시)에 남겨 다음 빌드에서 다시 받지 않습니다.
stage_files() {
  local list="$1" stage="$2" cache="$3" path sum url inner src dst tmp got
  local win_tar
  win_tar="$(cygpath -u "$SYSTEMROOT")/System32/tar.exe"
  rm -rf "${stage:?}"
  mkdir -p "$cache" "$stage"
  while read -r path sum url inner; do
    # Windows 체크아웃(autocrlf)이면 줄 끝에 CR이 붙어 마지막 칸(zip 안 경로·주소)이 어긋납니다.
    url="${url%$'\r'}"
    inner="${inner%$'\r'}"
    [[ -z "$path" || "$path" == "#"* ]] && continue
    if [[ "$url" == https://* ]]; then
      src="$cache/$(basename "$url")"
      if [[ ! -f "$src" ]]; then
        log "받기: $url"
        curl -fsSL --retry 3 -o "$src.part" "$url" && mv "$src.part" "$src"
      fi
    else
      # 저장소 안의 파일(vendor/ffmpeg처럼 직접 빌드해 넣어 둔 것).
      src="$url"
    fi
    dst="$stage/$path"
    mkdir -p "$(dirname "$dst")"
    if [[ -n "$inner" ]]; then
      tmp="$cache/x"
      rm -rf "$tmp"
      mkdir -p "$tmp"
      "$win_tar" -x -f "$(cygpath -w "$src")" -C "$(cygpath -w "$tmp")" "$inner"
      mv "$tmp/$inner" "$dst"
    else
      cp "$src" "$dst"
    fi
    got="$(sha256sum "$dst" | cut -d' ' -f1)"
    [[ "$got" == "$sum" ]] || fail "$path의 sha256이 다릅니다($got). 받은 파일($src)을 지우고 다시 빌드하세요"
  done < "$list"
}

# 신경망 음성 코덱(Mimi·DAC) 모델과 ONNX Runtime(수백 MB)은 실행 파일 안에 넣지 않고, 설치 파일(NSIS·MSI)과 포터블 zip에
# exe 옆 models 폴더로 함께 넣습니다(앱이 실행 파일 옆 models에서 엽니다, scripts/neural-models.txt).
models_stage="apps/desktop/src-tauri/models"
resources=()
if [[ -z "${NO_MODELS:-}" ]]; then
  step "신경망 코덱 모델 준비"
  stage_files scripts/neural-models.txt "$models_stage" "$dist/models-cache"
  cat > "$models_stage/README.txt" <<EOF
통화의 신경망 음성 코덱(Mimi·DAC) 모델과 ONNX Runtime입니다. 지우면 통화 음성은 Opus만 씁니다.
출처와 라이선스
  - Mimi: Kyutai(https://huggingface.co/kyutai/mimi), CC BY 4.0. 스트리밍 ONNX 변환: BMekiker/mimi-onnx-streaming(CC BY 4.0).
  - DAC 24kHz: Descript(https://github.com/descriptinc/descript-audio-codec), MIT. ONNX 변환: onnx-community/dac_24khz-ONNX.
  - ONNX Runtime 1.30.0: Microsoft, MIT(onnxruntime/LICENSE, ThirdPartyNotices.txt).
EOF
  resources+=('"models/"')
fi

# 화면공유 인코더 ffmpeg(직접 빌드한 LGPL 2.1+ 판, 약 21MB, 저장소의 vendor/ffmpeg)도 같은 방식으로 exe 옆 ffmpeg 폴더에
# 넣습니다. 앱은 띄우기 전에 같은 목록의 sha256으로 확인합니다(scripts/ffmpeg-files.txt).
ffmpeg_stage="apps/desktop/src-tauri/ffmpeg"
if [[ -z "${NO_FFMPEG:-}" ]]; then
  step "ffmpeg 준비"
  stage_files scripts/ffmpeg-files.txt "$ffmpeg_stage" "$dist/ffmpeg-cache"
  cat > "$ffmpeg_stage/README.txt" <<EOF
통화 화면공유의 ffmpeg 인코더(AOM AV1, SVT-AV1, NVIDIA NVENC AV1·H.264·HEVC)입니다. 지우면 화면공유는 웹뷰 인코더(VP8·VP9)만 씁니다.
출처와 라이선스
  - FFmpeg 9.0.2(https://ffmpeg.org), LGPL 2.1 이상(LICENSE.txt). 화면공유에 쓰는 것만 넣은 공유 라이브러리 빌드이고
    GPL·nonfree 부품은 없습니다. 함께 들어간 libaom 3.15.2, SVT-AV1 4.2.0(BSD 계열, AOM 특허 라이선스),
    nv-codec-headers 12.1.14.0(MIT), mingw-w64 런타임의 고지는 THIRD-PARTY-NOTICES.txt에 있습니다.
  - 소스: 각 프로젝트가 배포한 원본(LICENSE.txt·THIRD-PARTY-NOTICES.txt의 주소와 sha256). 이 폴더는 앱 저장소의
    scripts/build-ffmpeg.sh로 그 소스에서 같은 바이트로 다시 만들 수 있습니다. NVENC는 NVIDIA 드라이버 531.61 이상에서 됩니다.
  - 앱(yangpa-desktop.exe)은 ffmpeg.exe를 따로 실행할 뿐 FFmpeg 라이브러리를 링크하지 않습니다. 이 폴더의 DLL을 같은 판의 다른
    LGPL 빌드로 바꾸면 앱은 배포본과 다르다며 쓰지 않습니다(바뀐 실행 파일을 띄우지 않으려는 확인). 바꿔 쓰려면
    scripts/ffmpeg-files.txt의 sha256을 바꿔 앱을 다시 빌드하세요.
EOF
  resources+=('"ffmpeg/"')
fi

# Tor 브리지 전송 lyrebird(Tor 프로젝트 공식 배포본, 약 18MB)도 exe 옆 pt 폴더에 넣습니다. 데몬이 브리지를 켤 때
# 띄웁니다(scripts/lyrebird-files.txt).
pt_stage="apps/desktop/src-tauri/pt"
if [[ -z "${NO_LYREBIRD:-}" ]]; then
  step "lyrebird 준비"
  stage_files scripts/lyrebird-files.txt "$pt_stage" "$dist/pt-cache"
  cat > "$pt_stage/README.txt" <<EOF
Tor 브리지(검열 우회)의 전송 프로그램 lyrebird입니다. obfs4·Snowflake·WebTunnel 브리지를 켤 때 데몬이 띄웁니다.
지우면 브리지를 쓸 수 없습니다(브리지 없는 Tor는 그대로).
출처와 라이선스
  - lyrebird: Tor 프로젝트(https://gitlab.torproject.org/tpo/anti-censorship/pluggable-transports/lyrebird), BSD 계열.
    lyrebird와 함께 들어간 Go 라이브러리의 라이선스는 LICENSE.txt에 있습니다.
  - Tor Expert Bundle 16.0(https://archive.torproject.org/tor-package-archive/torbrowser/16.0/)에서 꺼낸 그대로이며,
    Tor Browser 개발자 서명을 확인한 묶음입니다.
EOF
  resources+=('"pt/"')
fi

# 라이선스 문서: 후원 모듈은 AGPL이 아니라 받는 사람이 두 라이선스와 7조 추가 조건을 함께 받아야 합니다.
licenses_stage="apps/desktop/src-tauri/licenses"
rm -rf "${licenses_stage:?}"
mkdir -p "$licenses_stage"
cp LICENSE NOTICE.md LICENSE-SUPPORTER.md TRADEMARK.md "$licenses_stage/"
resources+=('"licenses/"')

# 설치 파일에 리소스로 넣습니다(설치 폴더의 models·ffmpeg·pt·licenses, 실행 파일 옆).
tauri_extra=()
if ((${#resources[@]})); then
  tauri_extra=(--config "{\"bundle\":{\"resources\":[$(IFS=,; echo "${resources[*]}")]}}")
fi

step "데스크톱 앱·설치 파일($BUNDLES) 빌드"
(cd apps/desktop && npm run tauri -- build --ci --target "$TARGET" --bundles "$BUNDLES" "${tauri_extra[@]}" -- --locked)

if [[ -z "${NO_PORTABLE:-}" ]]; then
  step "포터블 zip 만들기"
  name="YANGPA_${VERSION}_x64_portable"
  pdir="target/portable/$SHA"
  # 같은 커밋으로 다시 만들 때만 덮어씁니다. 다른 커밋의 포터블 폴더는 실행 중일 수 있어 건드리지 않습니다.
  rm -rf "${pdir:?}/$name" "$pdir/$name.zip"
  mkdir -p "$pdir/$name"
  cp "$OUT/yangpa-desktop.exe" "$OUT/yangpad.exe" "$OUT/yangpa.exe" "$pdir/$name/"
  cp LICENSE "$pdir/$name/LICENSE.txt"
  cp NOTICE.md LICENSE-SUPPORTER.md TRADEMARK.md "$pdir/$name/"
  # 신경망 음성 코덱 모델은 설치본과 같이 exe 옆 models 폴더로.
  if [[ -z "${NO_MODELS:-}" ]]; then
    cp -r "$models_stage" "$pdir/$name/models"
  fi
  if [[ -z "${NO_FFMPEG:-}" ]]; then
    cp -r "$ffmpeg_stage" "$pdir/$name/ffmpeg"
  fi
  if [[ -z "${NO_LYREBIRD:-}" ]]; then
    cp -r "$pt_stage" "$pdir/$name/pt"
  fi
  cat > "$pdir/$name/README.txt" <<EOF
YANGPA $VERSION 포터블 ($SHA)

yangpa-desktop.exe를 실행하세요. 같은 폴더의 yangpad.exe(데몬)를 앱이 띄우고, 앱을 닫으면 데몬도 꺼집니다.
yangpa.exe는 명령줄 도구입니다.
YANGPA는 GNU AGPL v3 이상(LICENSE.txt)과 7조 추가 조건(NOTICE.md)으로 배포합니다. 후원 기능 코드만
YANGPA 후원 기능 라이선스(LICENSE-SUPPORTER.md)를 따르고, 이름·아이콘은 TRADEMARK.md를 따릅니다.
모든 데이터(대화·키·첨부·설정)는 기본으로 이 폴더의 Data 폴더에 둡니다. 다른 곳으로 바꾸려면 사용자 설정 → 저장 공간 →
데이터 폴더에서 바꾸세요(옮길 크기·예상 시간을 보고 데이터를 옮길지 고릅니다). 바꾼 위치는 이 폴더의 data-dir.json에
적히니 새 판으로 바꿀 때 Data 폴더와 함께 옮겨 두세요. 예전 판을 쓰던 데이터(%APPDATA%\yangpa\data)는 옮기지 않고 그대로
이어 씁니다(설정에 그 위치가 보이고, 원하면 같은 화면에서 Data 폴더로 옮길 수 있음).
models 폴더는 통화의 신경망 음성 코덱(Mimi·DAC) 모델과 ONNX Runtime입니다. exe 옆에 그대로 두세요(지우면 Opus만 씁니다).
모델 출처와 라이선스: Mimi(Kyutai, CC BY 4.0, 스트리밍 변환 BMekiker/mimi-onnx-streaming), DAC(Descript, MIT),
ONNX Runtime(Microsoft, MIT, models/onnxruntime/LICENSE).
ffmpeg 폴더는 화면공유의 ffmpeg 인코더(AOM AV1·SVT-AV1·NVENC)입니다. exe 옆에 그대로 두세요(지우면 웹뷰 인코더만 씁니다).
FFmpeg 9.0.2(LGPL 2.1 이상, 화면공유에 쓰는 것만 넣은 빌드, ffmpeg/LICENSE.txt·THIRD-PARTY-NOTICES.txt·README.txt).
pt 폴더는 Tor 브리지(검열 우회)의 전송 프로그램 lyrebird입니다. exe 옆에 그대로 두세요(지우면 브리지를 쓸 수 없습니다).
lyrebird(Tor 프로젝트, BSD 계열, Tor Expert Bundle 16.0에서 꺼낸 그대로, pt/LICENSE.txt·README.txt).
새 판으로 바꿀 때는 앱을 닫고, 작업 관리자에 예전 yangpad.exe가 남아 있지 않은지 확인한 뒤 새 폴더의 앱을 여세요.
서명하지 않은 파일이라 처음 실행할 때 Windows 경고가 뜰 수 있습니다.
EOF
  # Git bash의 tar(GNU)는 zip을 못 만들어 Windows의 bsdtar를 씁니다.
  (cd "$pdir" && "$(cygpath -u "$SYSTEMROOT")/System32/tar.exe" -a -c -f "$name.zip" "$name")
fi

step ""
shopt -s nullglob
for f in "$OUT"/bundle/nsis/YANGPA_"$VERSION"_*.exe "$OUT"/bundle/msi/YANGPA_"$VERSION"_*.msi \
  target/portable/"$SHA"/YANGPA_"$VERSION"_x64_portable.zip; do
  [[ -f "$f" ]] || continue
  size=$(( $(stat -c %s "$f") / 1024 / 1024 ))
  printf '  %s  %s MB  sha256 %s\n' "$(cygpath -aw "$f")" "$size" "$(sha256sum "$f" | cut -c1-16)…"
done
log "전체 $((SECONDS - START))초"
