#!/bin/sh
# 양파 메신저(가칭) deb 패키지의 postinst·postrm(tauri.conf.json의 bundle.linux.deb가 가리킴).
#
# 앱이 띄운 데몬(/usr/bin/yangpad, 사이드카)은 앱 창을 닫아도 시딩을 위해 돌고, 로그아웃해도 남을 수 있습니다.
# 리눅스는 실행 중인 파일을 바꿀 수 있어서 dpkg는 막히지 않지만, 그대로 두면
# - 업데이트 뒤에도 옛 판 데몬이 계속 돌고(보안 수정이 적용되지 않음), 새 앱이 그 옛 데몬에 붙습니다.
# - 제거 뒤에도 지워진 실행 파일로 계속 돌며 연결 상대에게 IP를 보입니다.
# 그래서 dpkg가 파일을 바꾸거나 지운 뒤(postinst configure, postrm remove·purge) "지워진 /usr/bin/yangpad"를 실행 중인
# 프로세스만 끕니다. 지금 설치된 실행 파일로 도는 데몬, 다른 경로의 데몬(CLI 묶음, /usr/local/bin의 헤드리스 시더)은
# 건드리지 않습니다. SIGTERM으로 정상 종료(DB 정리)를 기다리고, 10초 안에 끝나지 않으면 SIGKILL로 끝냅니다(Windows
# 설치 파일과 같음, DB는 SQLite 저널로 일관성 유지). 휘발성 채널 내용은 RAM에만 있어 함께 사라집니다.
# 열려 있던 앱은 데몬을 한 번만 띄우므로, 업데이트 뒤에는 앱을 다시 시작하면 새 데몬이 뜹니다.
# 실패해도 패키지 설치·제거는 막지 않습니다.

DAEMON=/usr/bin/yangpad

# 끝난(좀비 포함) 프로세스는 exe를 읽을 수 없고, 번호를 다른 프로세스가 다시 써도 경로가 달라 걸리지 않습니다.
is_stale() {
  [ "$(readlink "/proc/$1/exe" 2>/dev/null)" = "$DAEMON (deleted)" ]
}

stale_daemon_pids() {
  for d in /proc/[0-9]*; do
    if is_stale "${d#/proc/}"; then printf ' %s' "${d#/proc/}"; fi
  done
}

stop_stale_daemons() {
  pids="$(stale_daemon_pids)"
  [ -n "$pids" ] || return 0
  echo "YANGPA: 바뀌거나 지워진 데몬(yangpad)을 끕니다(PID$pids). 열려 있는 앱은 다시 시작하세요." >&2
  # shellcheck disable=SC2086 # PID 목록을 낱말로 나눔
  kill -TERM $pids 2>/dev/null || true
  i=0
  while [ "$i" -lt 10 ]; do
    alive=""
    for p in $pids; do
      if is_stale "$p"; then alive="$alive $p"; fi
    done
    [ -n "$alive" ] || return 0
    sleep 1
    i=$((i + 1))
  done
  echo "YANGPA: 10초 안에 끝나지 않은 데몬을 강제로 끝냅니다(PID$alive)." >&2
  # shellcheck disable=SC2086
  kill -KILL $alive 2>/dev/null || true
  return 0
}

case "$1" in
  configure | remove | purge) stop_stale_daemons ;;
esac

exit 0
