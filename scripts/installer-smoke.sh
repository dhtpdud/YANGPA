#!/usr/bin/env bash
# 릴리스 묶음 연기 시험(리눅스·macOS 릴리스 작업이 부름). Windows는 installer-smoke.ps1.
#
#   deb <파일.deb> <yangpa CLI>   설치 → 데몬 띄움 → 같은 묶음으로 다시 설치(파일 교체) → 옛 데몬이 정상 종료(SIGTERM)됐는지
#                                → 다시 띄움 → 제거 → 데몬이 끝나고 파일이 지워졌는지. sudo 필요(러너).
#   appimage <파일.AppImage>      풀어서 사이드카(usr/bin/yangpad)가 들어 있고 실행되는지.
#   dmg <파일.dmg> [아키텍처…]    붙여서 YANGPA.app에 앱·사이드카가 있고, 주어진 아키텍처(예: arm64 x86_64)를 모두
#                                담았고, 서명(임시 서명 포함)이 맞고, 데몬이 실행되는지.
#
# 데몬은 임시 데이터 경로·엔드포인트·127.0.0.1·mDNS 끔으로 띄우고, 로컬 API가 답할 때까지 기다립니다(고정 대기 없음).

set -euo pipefail

WAIT_SECS="${YP_WAIT_SECS:-120}"
WORK="$(mktemp -d "${RUNNER_TEMP:-/tmp}/yangpa-installer.XXXXXX")"
chmod 700 "$WORK"
DAEMON_PID=""

cleanup() {
  set +e
  if [[ -n "$DAEMON_PID" ]]; then kill "$DAEMON_PID" 2>/dev/null; fi
  if [[ -n "${MOUNT:-}" ]]; then hdiutil detach -quiet "$MOUNT" 2>/dev/null; fi
  rm -rf "$WORK"
}
trap cleanup EXIT

log() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }
fail() { echo "::error::$*" >&2; exit 1; }

# 데몬을 띄우고 로컬 API가 답할 때까지 기다립니다. DAEMON_PID·DAEMON_DIR를 채웁니다.
start_daemon() {
  local exe="$1" cli="$2" name="$3"
  DAEMON_DIR="$WORK/$name"
  mkdir -p "$DAEMON_DIR"
  chmod 700 "$DAEMON_DIR"
  "$exe" --data-dir "$DAEMON_DIR" --endpoint "$DAEMON_DIR/yangpad.sock" --key-storage file --no-mdns \
    --direct --listen /ip4/127.0.0.1/tcp/0 --log-file "$DAEMON_DIR/yangpad.log" --log info &
  DAEMON_PID=$!
  local deadline=$((SECONDS + WAIT_SECS))
  until "$cli" --endpoint "$DAEMON_DIR/yangpad.sock" --json --timeout 5s version 2>/dev/null | grep -q '"protocol"'; do
    alive "$DAEMON_PID" || fail "$name: 데몬이 바로 끝났습니다($(tail -n 20 "$DAEMON_DIR/yangpad.log" 2>/dev/null))"
    (( SECONDS < deadline )) || fail "$name: 데몬이 ${WAIT_SECS}초 안에 준비되지 않았습니다"
    sleep 0.5
  done
  log "$name: 데몬 준비(PID $DAEMON_PID)"
}

# 끝난 프로세스(좀비 포함)는 exe를 읽을 수 없습니다.
alive() { readlink "/proc/$1/exe" >/dev/null 2>&1 || { [[ "$(uname)" == Darwin ]] && kill -0 "$1" 2>/dev/null; }; }

expect_stopped() {
  local what="$1" deadline=$((SECONDS + 30))
  while alive "$DAEMON_PID"; do
    (( SECONDS < deadline )) || fail "$what: 데몬(PID $DAEMON_PID)이 30초 안에 끝나지 않았습니다"
    sleep 0.2
  done
  wait "$DAEMON_PID" 2>/dev/null || true
  DAEMON_PID=""
  # 정상 종료 경로(SIGTERM 처리)를 탔는지: 강제 종료면 이 줄이 없습니다.
  grep -q 'SIGTERM' "$DAEMON_DIR/yangpad.log" || fail "$what: 데몬이 정상 종료하지 않았습니다(SIGTERM 기록 없음)"
  log "$what: 데몬이 정상 종료함"
}

smoke_deb() {
  local deb cli pkg
  [[ -f "$1" ]] || fail "deb 묶음이 없습니다: $1"
  deb="$(realpath "$1")"
  cli="$(realpath "$2")"
  pkg="$(dpkg-deb -f "$deb" Package)"
  log "deb: $pkg 설치"
  sudo apt-get install -y -qq "$deb" >/dev/null
  [[ -x /usr/bin/yangpad && -x /usr/bin/yangpa-desktop ]] || fail "deb: /usr/bin에 앱·사이드카가 없습니다"
  /usr/bin/yangpad --version

  start_daemon /usr/bin/yangpad "$cli" deb-upgrade
  log "deb: 같은 묶음으로 다시 설치(실행 중인 파일 교체 → postinst)"
  sudo dpkg -i "$deb"
  expect_stopped "deb 업데이트"

  start_daemon /usr/bin/yangpad "$cli" deb-remove
  log "deb: 제거(postrm)"
  sudo dpkg -r "$pkg"
  expect_stopped "deb 제거"
  [[ ! -e /usr/bin/yangpad ]] || fail "deb: 제거 뒤에도 /usr/bin/yangpad가 남았습니다"
}

smoke_appimage() {
  local img
  [[ -f "$1" ]] || fail "AppImage 묶음이 없습니다: $1"
  img="$(realpath "$1")"
  chmod +x "$img"
  (cd "$WORK" && "$img" --appimage-extract >/dev/null)
  local bin="$WORK/squashfs-root/usr/bin"
  [[ -x "$bin/yangpad" && -x "$bin/yangpa-desktop" ]] || fail "AppImage: usr/bin에 앱·사이드카가 없습니다"
  "$bin/yangpad" --version
  log "AppImage: 사이드카 확인"
}

smoke_dmg() {
  local dmg="$1"; shift
  [[ -f "$dmg" ]] || fail "dmg 묶음이 없습니다: $dmg"
  MOUNT="$WORK/mnt"
  mkdir -p "$MOUNT"
  hdiutil attach -nobrowse -readonly -noautoopen -mountpoint "$MOUNT" "$dmg" >/dev/null
  local app="$MOUNT/YANGPA.app"
  local macos="$app/Contents/MacOS"
  [[ -x "$macos/yangpa-desktop" && -x "$macos/yangpad" ]] || fail "dmg: YANGPA.app/Contents/MacOS에 앱·사이드카가 없습니다"
  for b in yangpa-desktop yangpad; do
    local archs
    archs="$(lipo -archs "$macos/$b")"
    for a in "$@"; do
      [[ " $archs " == *" $a "* ]] || fail "dmg: $b에 $a가 없습니다(있는 것: $archs)"
    done
    log "dmg: $b 아키텍처 $archs"
  done
  codesign --verify --deep --strict --verbose=2 "$app"
  "$macos/yangpad" --version
  hdiutil detach -quiet "$MOUNT"
  MOUNT=""
}

case "${1:-}" in
  deb) smoke_deb "$2" "$3" ;;
  appimage) smoke_appimage "$2" ;;
  dmg) shift; smoke_dmg "$@" ;;
  *) echo "사용: $0 deb <파일> <yangpa> | appimage <파일> | dmg <파일> [아키텍처…]" >&2; exit 2 ;;
esac
