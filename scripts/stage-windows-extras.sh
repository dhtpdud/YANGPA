#!/usr/bin/env bash
# Windows 설치 파일·포터블에 실행 파일 옆으로 넣을 것(신경망 코덱 models, 화면공유 ffmpeg, 브리지 lyrebird pt, 라이선스)을
# apps/desktop/src-tauri 아래에 준비하고, tauri `--config`의 `bundle.resources`로 넘길 JSON 배열을 표준 출력에 씁니다.
# scripts/build-windows-release.sh(PC 빌드)와 .github/workflows/release.yml(Windows 작업)이 함께 써서 두 배포본이 같게 합니다.
#
#   scripts/stage-windows-extras.sh <캐시 폴더>   받은 파일을 그 폴더에 남겨 다음에 다시 받지 않음
#   NO_MODELS=1 · NO_FFMPEG=1 · NO_LYREBIRD=1 …   그 폴더를 넣지 않음(라이선스는 늘 넣음)
#
# Windows(Git bash)에서만 돌립니다(zip은 Windows의 tar.exe로 풂). 진행 기록은 표준 오류로 씁니다.
set -euo pipefail

cache="${1:?캐시 폴더를 주세요}"
mkdir -p "$cache"
cache="$(cd "$cache" && pwd)"
cd "$(dirname "${BASH_SOURCE[0]}")/.."

log() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*" >&2; }
fail() { echo "오류: $*" >&2; exit 1; }

# 목록(<폴더 안 경로> <sha256> <주소> [zip 안 경로])의 파일을 받아 단계 폴더에 놓고 sha256을 맞춰 봅니다. 받은 파일은
# 캐시 폴더에 남겨 다음 빌드에서 다시 받지 않습니다.
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
ffmpeg_stage="apps/desktop/src-tauri/ffmpeg"
pt_stage="apps/desktop/src-tauri/pt"
licenses_stage="apps/desktop/src-tauri/licenses"
resources=()
if [[ -z "${NO_MODELS:-}" ]]; then
  log "신경망 코덱 모델 준비"
  stage_files scripts/neural-models.txt "$models_stage" "$cache/models-cache"
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
if [[ -z "${NO_FFMPEG:-}" ]]; then
  log "ffmpeg 준비"
  stage_files scripts/ffmpeg-files.txt "$ffmpeg_stage" "$cache/ffmpeg-cache"
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
if [[ -z "${NO_LYREBIRD:-}" ]]; then
  log "lyrebird 준비"
  stage_files scripts/lyrebird-files.txt "$pt_stage" "$cache/pt-cache"
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
rm -rf "${licenses_stage:?}"
mkdir -p "$licenses_stage"
cp LICENSE NOTICE.md LICENSE-SUPPORTER.md TRADEMARK.md "$licenses_stage/"
resources+=('"licenses/"')

# tauri --config의 bundle.resources로 넘길 배열(설치 폴더의 models·ffmpeg·pt·licenses, 실행 파일 옆).
echo "[$(IFS=,; echo "${resources[*]}")]"
