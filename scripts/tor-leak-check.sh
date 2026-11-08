#!/usr/bin/env bash
# Tor 실제 망 누출 검사(리눅스, root 필요).
#
# 1. 네트워크 이름공간(netns) 하나를 만들어 veth로 바깥에 NAT 연결하고(IPv6 끔), 바깥쪽 veth에서 tcpdump로
#    이름공간이 내보내는 패킷을 모두 잡습니다. 이름공간의 DNS 서버는 바깥쪽 veth 주소라 DNS 조회가 있으면 잡힙니다.
# 2. 그 안에서 yangpad 두 개를 기본(Tor)으로 띄우고 yangpa CLI로 실제 Tor 망 위의 일을 시킵니다. 단계마다 시각을
#    phases.tsv에 적어 분석기가 어느 단계에서 샜는지 나눠 보여 줍니다.
#    - 평문 부트스트랩 주소(b를 숨은 옵션 `--bootstrap`으로 띄움, 출구 노드로만 나가야 함)
#    - 친구: onion 링크로 친구 맺기, 1:1 메시지(링크 미리보기 포함)·첨부·답장
#    - 크루: 만들기, 음성 채널, 초대 링크, b가 onion으로 가입, 크루 메시지(링크 미리보기)·첨부, a의 답장
#    - 통화: P2P 전용이 아닌 채널이고 신뢰를 켜지 않았으니 Tor 통화(WebRTC 없음, ICE 서버 없음)로 참가하고, a가 소리
#      패킷(`voice.media`)을 보냅니다. 패킷은 노드 연결(Tor 회로)로만 가고 SFU 호스트도 맡지 않으며 STUN 서버에도 묻지
#      않으므로 UDP는 하나도 나가면 안 됩니다.
#    - 내 기기(맨 뒤): 새 기기 c를 a에 잇습니다. 링크를 띄운 때부터 a·c는 설계대로 Tor 밖으로 STUN·UPnP를 씁니다
#      (decisions.md "첫 동기화와 내 기기 직접 연결"). 그래서 앞 단계가 기기 1대 계정의 판정이 되게 맨 뒤에 둡니다.
# 3. 끝나면 캡처를 scripts/tor_leak_analyze.py로 판정합니다: 나간 패킷의 목적지가 지금 합의 문서의 Tor 릴레이
#    (또는 Arti에 내장된 폴백 디렉터리·권한)가 아니면 실패(UDP·DNS·IPv6는 모두 실패). 내 기기 단계부터만 STUN 목록의
#    서버, UPnP 찾기, STUN이 알려 준 내 공인 IP로 가는 UDP를 따로 세어 넘기고, 그마저 없으면 실패합니다(그 길을 밟지
#    못한 것). 데몬 로그에 메시지 본문·친구·초대·기기 연결 링크가 남아도 실패.
#    브리지는 이 시나리오 밖입니다: 브리지 없이 돌고, lyrebird가 없으면 자동 Snowflake도 켜지지 않습니다.
#
# 사용: sudo scripts/tor-leak-check.sh [--bin-dir target/debug] [--out 결과경로] [--keep]
#         --out은 윗 폴더까지 root 소유여야 합니다(아니면 Arti가 권한 검사에서 멈춰 Tor가 시작하지 못함).
#       scripts/tor-leak-check.sh --no-tor [--bin-dir target/debug] [--keep]
#         Tor·root·캡처 없이 같은 시나리오를 127.0.0.1에서만 돌려 스크립트·API 흐름을 점검합니다(판정 없음,
#         직접 연결이라 통화는 참가되고 서로 보여야 함).
# 환경 변수: YP_WAIT_SECS(단계별 최대 대기, 기본 1200·--no-tor 120), YP_PREVIEW_URL(기본 https://example.com/,
#           --no-tor는 기본 없음), YP_DIRCOMMON(tor-dircommon 소스 경로, 없으면 cargo metadata로 찾음).
#
# 검사 함수 여럿은 wait_for를 거쳐서만 불립니다.
# shellcheck disable=SC2329

set -euo pipefail

BIN_DIR="target/debug"
OUT=""
KEEP=0
TOR=1
while [[ $# -gt 0 ]]; do
  case "$1" in
    --bin-dir) BIN_DIR="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --keep) KEEP=1; shift ;;
    --no-tor) TOR=0; shift ;;
    -h|--help) sed -n '2,28p' "$0"; exit 0 ;;
    *) echo "알 수 없는 인자: $1" >&2; exit 2 ;;
  esac
done

if [[ $TOR -eq 1 ]]; then
  if [[ $(id -u) -ne 0 ]]; then
    echo "root로 실행해야 합니다(netns·tcpdump). Tor 없이 시나리오만 보려면 --no-tor" >&2
    exit 2
  fi
  for t in ip iptables tcpdump python3; do
    command -v "$t" >/dev/null || { echo "필요한 도구가 없음: $t" >&2; exit 2; }
  done
else
  command -v python3 >/dev/null || { echo "필요한 도구가 없음: python3" >&2; exit 2; }
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
BIN_DIR="$(cd "$BIN_DIR" && pwd)"
YANGPAD="$BIN_DIR/yangpad"
YANGPA="$BIN_DIR/yangpa"
[[ -x "$YANGPAD" && -x "$YANGPA" ]] || { echo "$BIN_DIR에 yangpad·yangpa가 없습니다(cargo build -p yangpa-daemon -p yangpa-cli)" >&2; exit 2; }

if [[ $TOR -eq 1 ]]; then
  WAIT_SECS="${YP_WAIT_SECS:-1200}"
  PREVIEW_URL="${YP_PREVIEW_URL-https://example.com/}"
  POLL=5
else
  WAIT_SECS="${YP_WAIT_SECS:-120}"
  # 루프백 점검이 바깥 사이트에 직접 접속하지 않게 기본은 미리보기 없음.
  PREVIEW_URL="${YP_PREVIEW_URL:-}"
  POLL=0.5
fi
NS="ypleak$$"
VH="yph$$"
VN="ypn$$"
HOST_IP="10.231.0.1"
NS_IP="10.231.0.2"
WORK="${OUT:-$(mktemp -d "${TMPDIR:-/tmp}/yangpa-tor-leak.XXXXXX")}"
mkdir -p "$WORK"
chmod 700 "$WORK"
# 유닉스 소켓 경로 한도(108바이트) 안에 들게, 작업 경로가 길면 소켓만 짧은 임시 폴더에 둡니다.
SOCK_DIR="$WORK"
if (( ${#WORK} > 80 )); then
  SOCK_DIR="$(mktemp -d /tmp/yps.XXXXXX)"
  chmod 700 "$SOCK_DIR"
fi
PCAP="$WORK/capture.pcap"
# 단계 표시(분석기 --phases)와 시나리오 기록. 둘 다 내용·키·링크 없이 단계 이름과 수만 적습니다.
PHASES="$WORK/phases.tsv"
SCENARIO="$WORK/scenario.txt"
# 내 기기 단계부터 허용하는 `IP:포트`(UPnP 찾기, STUN 서버). 내용은 공개 주소뿐입니다.
DIRECT_ALLOW="$WORK/direct-allow.txt"
: > "$PHASES"
: > "$SCENARIO"
# 데몬·CLI를 이름공간 안에서 돌리는 접두사(--no-tor면 없음).
NSX=()
UPLINK=""
# 돌린 기계의 설정을 바꿔 둔 채 끝나지 않게 원래 값을 기억합니다.
FORWARD_WAS=""
if [[ $TOR -eq 1 ]]; then
  NSX=(ip netns exec "$NS")
  UPLINK="$(ip -4 route show default | awk '{print $5; exit}')"
  [[ -n "$UPLINK" ]] || { echo "기본 경로가 없습니다" >&2; exit 2; }
fi

PIDS=()
CAPTURE_PID=""
cleanup() {
  set +e
  local all=("${PIDS[@]}")
  if [[ -n "$CAPTURE_PID" ]]; then all+=("$CAPTURE_PID"); fi
  for p in "${all[@]}"; do kill "$p" 2>/dev/null; done
  for p in "${all[@]}"; do wait "$p" 2>/dev/null; done
  if [[ $TOR -eq 1 ]]; then
    iptables -t nat -D POSTROUTING -s "$NS_IP/32" -o "$UPLINK" -j MASQUERADE 2>/dev/null
    iptables -D FORWARD -i "$VH" -o "$UPLINK" -j ACCEPT 2>/dev/null
    iptables -D FORWARD -i "$UPLINK" -o "$VH" -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT 2>/dev/null
    ip link del "$VH" 2>/dev/null
    ip netns del "$NS" 2>/dev/null
    rm -rf "/etc/netns/$NS"
    if [[ -n "$FORWARD_WAS" ]]; then sysctl -qw net.ipv4.ip_forward="$FORWARD_WAS"; fi
  fi
  if [[ "$SOCK_DIR" != "$WORK" ]]; then rm -rf "$SOCK_DIR"; fi
  if [[ $KEEP -eq 0 && -z "$OUT" ]]; then rm -rf "$WORK"; fi
}
trap cleanup EXIT

log() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }
phase() { printf '%s\t%s\n' "$(date +%s.%N)" "$1" >> "$PHASES"; log "── 단계: $1"; }
# 시나리오 결과 한 줄(`이름=값`). 내용·키·링크는 넣지 않습니다.
note() { printf '%s\n' "$1" >> "$SCENARIO"; log "기록: $1"; }
warn() {
  log "경고: $1"
  if [[ -n "${GITHUB_ACTIONS:-}" ]]; then echo "::warning::$1"; fi
}

if [[ $TOR -eq 1 ]]; then
  # ───── 이름공간과 NAT ─────
  log "이름공간 $NS 준비(바깥 연결: $UPLINK)"
  ip netns add "$NS"
  mkdir -p "/etc/netns/$NS"
  # DNS 조회는 바깥쪽 veth로 가게 해 잡히도록(아무도 답하지 않음).
  echo "nameserver $HOST_IP" > "/etc/netns/$NS/resolv.conf"
  # 기본 STUN 서버 이름은 hosts로 풉니다. DNS에 답이 없으면 STUN이 나가지 못하고, 이렇게 두면 나가는 DNS 조회는 다 누출입니다.
  cp /etc/hosts "/etc/netns/$NS/hosts"
  echo "239.255.255.250:1900" > "$DIRECT_ALLOW"
  while read -r s; do
    stun_ip="$(getent ahostsv4 "${s%:*}" | awk 'NR==1{print $1}')"
    [[ -n "$stun_ip" ]] || continue
    echo "$stun_ip ${s%:*}" >> "/etc/netns/$NS/hosts"
    echo "$stun_ip:${s##*:}" >> "$DIRECT_ALLOW"
  done < <(sed -n '/const DEFAULT_STUN_SERVERS/,/];/s/.*"\(.*\)".*/\1/p' "$REPO_DIR/crates/yangpa-daemon/src/config.rs")
  ip link add "$VH" type veth peer name "$VN"
  ip link set "$VN" netns "$NS"
  ip addr add "$HOST_IP/30" dev "$VH"
  ip link set "$VH" up
  ip netns exec "$NS" ip addr add "$NS_IP/30" dev "$VN"
  ip netns exec "$NS" ip link set "$VN" up
  ip netns exec "$NS" ip link set lo up
  ip netns exec "$NS" ip route add default via "$HOST_IP"
  # IPv6가 없는 커널(설정 파일이 없음)이면 끌 것도 없습니다. 분석기는 IPv6 패킷을 모두 누출로 봅니다.
  if ip netns exec "$NS" test -d /proc/sys/net/ipv6; then
    ip netns exec "$NS" sysctl -qw net.ipv6.conf.all.disable_ipv6=1
    ip netns exec "$NS" sysctl -qw net.ipv6.conf.default.disable_ipv6=1
  fi
  FORWARD_WAS="$(sysctl -n net.ipv4.ip_forward)"
  sysctl -qw net.ipv4.ip_forward=1
  iptables -t nat -A POSTROUTING -s "$NS_IP/32" -o "$UPLINK" -j MASQUERADE
  # Docker가 있으면 FORWARD 기본 정책이 DROP이므로 앞에 넣습니다.
  iptables -I FORWARD 1 -i "$VH" -o "$UPLINK" -j ACCEPT
  iptables -I FORWARD 1 -i "$UPLINK" -o "$VH" -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT

  # ───── 캡처 ─────
  log "캡처 시작: $PCAP"
  # 256: 분석기가 STUN 답의 속성까지 읽게.
  tcpdump -i "$VH" -n -U -s 256 -w "$PCAP" 'not arp' 2>"$WORK/tcpdump.log" &
  CAPTURE_PID=$!
  # tcpdump가 실제로 듣기 시작할 때까지(파일 머리가 써짐).
  for _ in $(seq 1 100); do
    [[ -s "$PCAP" ]] && break
    sleep 0.1
  done
  [[ -s "$PCAP" ]] || { echo "tcpdump가 시작하지 않음" >&2; cat "$WORK/tcpdump.log" >&2; exit 1; }
fi

# ───── 데몬 ─────
# start_node <이름> [더 넘길 yangpad 인자…]
start_node() {
  local name="$1"
  shift
  local dir="$WORK/$name"
  mkdir -p "$dir"
  chmod 700 "$dir"
  local net_args=(--sync-interval 30s)
  # 루프백: 고정 포트 없이 127.0.0.1에서 듣고, DHT가 없으니 따라잡기를 짧게(데스크톱 e2e 시험과 같음). 직접 스웜도
  # 주소를 정해 줘야 내 기기 단계가 바깥 STUN·공유기에 묻지 않습니다.
  [[ $TOR -eq 1 ]] || net_args=(--direct --listen /ip4/127.0.0.1/tcp/0 --p2p-listen /ip4/127.0.0.1/tcp/0 --sync-interval 2s)
  "${NSX[@]}" env YANGPA_DATA_DIR="$dir" "$YANGPAD" \
    --data-dir "$dir" --endpoint "$SOCK_DIR/$name.sock" "${net_args[@]}" --no-mdns \
    --key-storage file --log-file "$dir/yangpad.log" --log info "$@" &
  PIDS+=($!)
}

rpc() {
  local name="$1" method="$2" params="${3:-}"
  [[ -n "$params" ]] || params='{}'
  local dir="$WORK/$name"
  "${NSX[@]}" env YANGPA_DATA_DIR="$dir" YANGPA_ENDPOINT="$SOCK_DIR/$name.sock" \
    "$YANGPA" --json --timeout 300s call "$method" "$params"
}

# 오류 코드(성공이면 ok). 성공 출력은 버립니다(통화 참가 결과의 프레임 키가 어디에도 남지 않게).
rpc_code() {
  local err
  if err="$(rpc "$@" 2>&1 >/dev/null)"; then
    echo ok
    return
  fi
  printf '%s\n' "$err" | tail -n 1 | jget 'd["error"].get("code", "?")' 2>/dev/null || echo "?"
}

# JSON 표준 입력에서 파이썬 식(`d`가 값)을 계산해 출력합니다.
jget() { python3 -I -c "import json,sys; d=json.load(sys.stdin); v=($1); print('' if v is None else (v if isinstance(v,str) else json.dumps(v)))"; }

# 데몬 프로세스가 살아 있는지(끝나고 거두지 않은 좀비도 끝난 것으로 봄).
node_alive() { local s; s="$(cut -d' ' -f3 "/proc/$1/stat" 2>/dev/null)" && [[ -n "$s" && "$s" != Z ]]; }

wait_for() {
  local what="$1"; shift
  local deadline=$((SECONDS + WAIT_SECS)) p
  until "$@" >/dev/null 2>&1; do
    for p in "${PIDS[@]}"; do
      node_alive "$p" || { echo "데몬(PID $p)이 끝나 기다리지 않습니다: $what" >&2; return 1; }
    done
    if (( SECONDS > deadline )); then
      echo "시간 초과: $what" >&2
      return 1
    fi
    sleep "$POLL"
  done
  log "됨: $what"
}

# 실제 망 위의 일. 실패해도 캡처는 판정합니다(어디까지 했는지는 로그·scenario.txt로).
exercise() {
  wait_for "a API" rpc a system.status || return 1
  wait_for "b API" rpc b system.status || return 1
  rpc a identity.create '{"name":"a"}' >/dev/null || return 1
  rpc b identity.create '{"name":"b"}' >/dev/null || return 1
  if [[ $TOR -eq 1 ]]; then
    wait_for "a Tor 부트스트랩" tor_ready a || return 1
    wait_for "b Tor 부트스트랩" tor_ready b || return 1
  else
    wait_for "a·b가 서로 연결됨" meshed || return 1
  fi

  phase friend
  # 친구 맺기(onion 링크). onion 서비스 게시에 몇 분 걸릴 수 있어 같은 링크로 다시 보냅니다.
  LINK="$(rpc a friend.link.create '{"expires_in_secs":7200,"single_use":false}' | jget 'd["url"]')" || return 1
  wait_for "b → a 연락처 요청" add_friend || return 1
  # 링크 주인은 사람의 수락 없이 임시 연락처로 받아 1:1 공간을 만듭니다.
  wait_for "a에게 b가 연락처로 생김" contact_space || return 1
  SPACE="$(contact_space)" || return 1
  wait_for "b가 1:1 공간을 받음" has_space || return 1
  CHANNEL="$(rpc b channel.list "{\"space\":\"$SPACE\"}" | jget 'd[0]["channel"]')" || return 1
  note "friend=ok"

  phase dm
  # 메시지(링크 미리보기는 b가 Tor 출구로 조회)·첨부(a가 onion으로 받음).
  send_with_attachment b "$SPACE" "$CHANNEL" dm || return 1
  rpc a message.send "{\"space\":\"$SPACE\",\"channel\":\"$CHANNEL\",\"text\":\"$SECRET_TEXT 답장\"}" >/dev/null || return 1

  phase crew
  CREW="$(rpc a crew.create '{"name":"누출 검사 크루"}' | jget 'd["space"]')" || return 1
  CREW_TEXT="$(rpc a channel.list "{\"space\":\"$CREW\"}" | jget 'next(c["channel"] for c in d if c["settings"]["kind"] == "text")')" || return 1
  VOICE="$(rpc a channel.create "{\"space\":\"$CREW\",\"name\":\"음성\",\"kind\":\"voice\"}" | jget 'd["channel"]')" || return 1
  INVITE="$(rpc a invite.create "{\"space\":\"$CREW\",\"expires_in_secs\":7200}" | jget 'd["url"]')" || return 1
  # 닿지 못하면(1009) 같은 링크로 다시 시도합니다. pending이면 데몬이 웰컴을 따라잡습니다.
  wait_for "b가 초대 링크로 가입 요청" join_crew || return 1
  wait_for "b가 크루 멤버가 됨" crew_member b || return 1
  wait_for "b가 크루 채널을 봄" crew_channel_visible b || return 1
  note "crew_join=ok"

  phase crew_message
  send_with_attachment b "$CREW" "$CREW_TEXT" crew || return 1
  local reply
  reply="$(rpc a message.send "{\"space\":\"$CREW\",\"channel\":\"$CREW_TEXT\",\"text\":\"$SECRET_TEXT 크루 답장\"}" | jget 'd["id"]')" || return 1
  wait_for "b가 크루 답장을 받음" got_msg b "$CREW" "$CREW_TEXT" "$reply" || return 1

  if [[ $TOR -eq 1 ]]; then
    phase voice_tor
    voice_tor || return 1
  else
    phase voice
    voice_direct || return 1
  fi

  phase own_device
  own_device || return 1
}

# Tor로 돌면 P2P 전용이 아닌 채널의 통화는 Tor 통화여야 합니다: 토폴로지 tor, ICE 서버 없음, 서로 보이고 프레임 키를 받음.
# 그다음 a가 소리 패킷을 보냅니다(캡처가 Tor 릴레이 밖으로 나간 패킷이 없는지 봄).
voice_tor() {
  local n view call i data
  for n in a b; do
    [[ "$(rpc_code "$n" voice.join "$(voice_params)")" == ok ]] || { echo "$n: Tor 통화 참가 실패" >&2; return 1; }
    view="$(rpc "$n" voice.join "$(voice_params)" | jget '"%s %d" % (d["topology"], len(d.get("ice_servers", [])))')" || return 1
    [[ "$view" == "tor 0" ]] || { echo "$n: Tor 통화가 아니거나 ICE 서버가 있음($view)" >&2; return 1; }
  done
  wait_for "a·b가 Tor 통화에서 서로 보이고 프레임 키를 받음" voice_both_ready || return 1
  call="$(rpc a voice.join "$(voice_params)" | jget 'd["call"]')" || return 1
  for i in $(seq 1 40); do
    data="$(head -c 120 /dev/urandom | base64 -w0)"
    rpc a voice.media "{\"space\":\"$CREW\",\"channel\":\"$VOICE\",\"call\":\"$call\",\"data\":\"$data\"}" >/dev/null       || { echo "a: 소리 패킷 $i 보내기 실패" >&2; return 1; }
  done
  note "voice_on_tor=tor"
  for n in a b; do rpc "$n" voice.leave "$(voice_params)" >/dev/null || true; done
}

# 루프백(--direct)은 Tor 통화가 아니라 참가되고 ICE 서버를 받습니다(`stun::call_ice`). 내 STUN 목록(`net.stun_servers`)만
# 와야 하고, 서로 보이고 프레임 키를 받아야 합니다. CLI는 WebRTC가 없어 STUN을 쓰지 않습니다.
voice_direct() {
  local n got want
  for n in a b; do
    [[ "$(rpc_code "$n" voice.join "$(voice_params)")" == ok ]] || { echo "$n: 직접 연결 통화 참가 실패" >&2; return 1; }
    got="$(rpc "$n" voice.join "$(voice_params)" | jget 'json.dumps([u for s in d.get("ice_servers", []) for u in s["urls"]])')" || return 1
    want="$(rpc "$n" net.stun_servers | jget '["stun:" + s for s in d["servers"]]')" || return 1
    [[ "$got" == "$want" ]] || { echo "$n: ICE 서버가 STUN 목록과 다름($got)" >&2; return 1; }
  done
  wait_for "a·b가 통화에서 서로 보이고 프레임 키를 받음" voice_both_ready || return 1
  note "voice_direct=all"
  for n in a b; do rpc "$n" voice.leave "$(voice_params)" >/dev/null || true; done
}

# a가 링크를 띄우고 새 기기 c가 그 링크로 들어와 첫 동기화를 끝냅니다. 사람의 승인은 스크립트가 대신합니다.
own_device() {
  DEVICE_LINK="$(rpc a device.link.create | jget 'd["url"]')" || return 1
  start_node c "${BOOT[@]}"
  wait_for "c API" rpc c system.status || return 1
  if [[ $TOR -eq 1 ]]; then wait_for "c Tor 부트스트랩" tor_ready c || return 1; fi
  rpc c device.link.join "{\"url\":\"$DEVICE_LINK\"}" >/dev/null || return 1
  wait_for "a에 c의 연결 요청이 옴" device_requested || return 1
  rpc a device.link.decide '{"approve":true}' >/dev/null || return 1
  wait_for "c가 첫 동기화를 끝냄" first_synced || return 1
  note "own_device=ok"
}

# b가 메시지(본문 + 미리보기 링크 + 첨부)를 보내고 a가 받아 첨부까지 내려받습니다.
send_with_attachment() {
  local from="$1" space="$2" channel="$3" tag="$4"
  local file="$WORK/$from/첨부-$tag.bin" msg blob embeds
  head -c 200000 /dev/urandom > "$file"
  msg="$(rpc "$from" message.send "{\"space\":\"$space\",\"channel\":\"$channel\",\"text\":\"$SECRET_TEXT $tag ${PREVIEW_URL}\",\"attachments\":[{\"path\":\"$file\"}]}" | jget 'd["id"]')" || return 1
  if [[ -n "$PREVIEW_URL" ]]; then
    embeds="$(list_msgs "$from" "$space" "$channel" | jget "next((len(m.get('embeds', [])) for m in d['messages'] if m['id'] == '$msg'), 0)")" || return 1
    note "${tag}_link_previews=$embeds"
    # 0이어도 실패로 보지 않음(출구·사이트 사정). 조회 시도의 패킷은 그래도 판정됩니다.
    [[ "$embeds" != 0 ]] || warn "$tag: 링크 미리보기를 만들지 못했습니다(Tor 출구·사이트 사정일 수 있음)"
  fi
  wait_for "a가 $tag 메시지를 받음" got_msg a "$space" "$channel" "$msg" || return 1
  blob="$(list_msgs a "$space" "$channel" | jget "next((m['attachments'][0]['blob'] for m in d['messages'] if m['id'] == '$msg'), None)")" || return 1
  [[ -n "$blob" ]] || { echo "$tag: 받은 메시지에 첨부가 없음" >&2; return 1; }
  rpc a blob.fetch "{\"space\":\"$space\",\"blob\":\"$blob\",\"priority\":255}" >/dev/null || true
  wait_for "a가 $tag 첨부를 받음" blob_done a "$space" "$blob" || return 1
  note "${tag}_attachment=ok"
}

tor_ready() { [[ "$(rpc "$1" net.status | jget 'd.get("bootstrap_progress")')" == "100" ]]; }
# 다른 데몬이 붙을 수 있는 주소(`/p2p/<id>` 포함). 루프백 점검에서만.
dial_addrs() { rpc "$1" net.status | jget '"\n".join(a if "/p2p/" in a else a + "/p2p/" + d["peer_id"] for a in d["listen_addrs"]) or None'; }
peer_id() { rpc "$1" net.status | jget 'd["peer_id"]'; }
sees_peer() { [[ -n "$(rpc "$1" net.peers | jget "next((p['peer_id'] for p in d if p['peer_id'] == '$2'), None)")" ]]; }
meshed() { sees_peer a "$(peer_id b)" && sees_peer b "$(peer_id a)"; }
add_friend() { rpc b friend.add "{\"url\":\"$LINK\"}"; }
contact_space() {
  local s
  s="$(rpc a friend.list | jget 'next((f["space"] for f in d if f.get("space")), None)')" && [[ -n "$s" ]] && printf '%s\n' "$s"
}
has_space() { [[ -n "$(rpc b friend.list | jget "next((f['space'] for f in d if f['space'] == '$SPACE'), None)")" ]]; }
join_crew() { rpc b invite.join "{\"url\":\"$INVITE\"}"; }
crew_member() { [[ -n "$(rpc "$1" space.list | jget "next((s['space'] for s in d if s['space'] == '$CREW'), None)")" ]]; }
crew_channel_visible() { [[ -n "$(rpc "$1" channel.list "{\"space\":\"$CREW\"}" | jget "next((c['channel'] for c in d if c['channel'] == '$CREW_TEXT'), None)")" ]]; }
list_msgs() { rpc "$1" message.list "{\"space\":\"$2\",\"channel\":\"$3\",\"limit\":20}"; }
got_msg() { [[ -n "$(list_msgs "$1" "$2" "$3" | jget "next((m['id'] for m in d['messages'] if m['id'] == '$4'), None)")" ]]; }
blob_done() { rpc "$1" blob.status "{\"space\":\"$2\",\"blob\":\"$3\"}" | jget 'd["total_chunks"] > 0 and d["have_chunks"] == d["total_chunks"] or None' | grep -q true; }
device_requested() { [[ -n "$(rpc a device.link.pending | jget 'd and d["code"]')" ]]; }
first_synced() { [[ "$(rpc c identity.get | jget 'd["first_syncing"]')" == false ]]; }
voice_params() { printf '{"space":"%s","channel":"%s"}' "$CREW" "$VOICE"; }
# 통화 참가(이미 참가 중이면 지금 상태)의 요약 한 줄 "참가자수 세대 ICE서버수". 프레임 키는 내보내지 않습니다.
voice_view() {
  rpc "$1" voice.join "$(voice_params)" | jget '"%d %d %d" % (len(d["participants"]), d["generation"], len(d.get("ice_servers", [])))'
}
voice_ready() {
  local n gen
  read -r n gen _ <<<"$(voice_view "$1")" || return 1
  [[ "$n" == 2 && "$gen" -gt 0 ]]
}
voice_both_ready() { voice_ready a && voice_ready b; }

LINK="" SPACE="" CHANNEL="" CREW="" CREW_TEXT="" VOICE="" INVITE="" DEVICE_LINK=""
BOOT=()
# 로그에 남으면 안 되는 것(메시지 본문).
SECRET_TEXT="Tor로 보냄 $$-$RANDOM"
phase start
start_node a
if [[ $TOR -eq 1 ]]; then
  # 평문 부트스트랩 주소: Tor로 돌면 출구 노드를 거쳐야 합니다(상시 노드 설정은 없어져 시작 인자로만 줌).
  start_node b --bootstrap /dns4/example.com/tcp/4001/p2p/12D3KooWD3eckifWpRn9wQpMG9R9hX3sD158z7EqHWmweQAJU5SA
else
  # 루프백: 런타임에 주소를 더하는 API가 없으므로 b를 a의 주소로 띄워 잇습니다.
  wait_for "a API" rpc a system.status || exit 1
  wait_for "a 수신 주소" dial_addrs a || exit 1
  while read -r addr; do BOOT+=(--bootstrap "$addr"); done <<<"$(dial_addrs a)"
  start_node b "${BOOT[@]}"
fi
EXERCISED=1
exercise || EXERCISED=0
phase shutdown

# ───── 정리·판정 ─────
log "데몬을 멈추고 캡처를 닫습니다"
for p in "${PIDS[@]}"; do kill -INT "$p" 2>/dev/null || true; done
for p in "${PIDS[@]}"; do wait "$p" 2>/dev/null || true; done
PIDS=()
if [[ -n "$CAPTURE_PID" ]]; then
  kill -INT "$CAPTURE_PID" 2>/dev/null || true
  wait "$CAPTURE_PID" 2>/dev/null || true
  CAPTURE_PID=""
fi

STATUS=0
if [[ $TOR -eq 1 ]]; then
  # 폴백 디렉터리·권한 주소: 빌드에 쓴 tor-dircommon 소스(sudo로 cargo가 PATH에 없을 수 있어 YP_DIRCOMMON 우선).
  DIRCOMMON="${YP_DIRCOMMON:-$(cd "$REPO_DIR" && cargo metadata --format-version 1 2>/dev/null | python3 -I -c 'import json,os,sys; m=json.load(sys.stdin); print(next(os.path.dirname(p["manifest_path"]) for p in m["packages"] if p["name"]=="tor-dircommon"))' || true)}"
  [[ -n "$DIRCOMMON" ]] || log "경고: tor-dircommon 소스를 찾지 못해 폴백 디렉터리 주소 없이 판정합니다"
  set +e
  python3 -I "$SCRIPT_DIR/tor_leak_analyze.py" analyze \
    --pcap "$PCAP" --local "$NS_IP" \
    --consensus-dir "$WORK/a/arti/cache/dir_blobs" \
    --consensus-dir "$WORK/b/arti/cache/dir_blobs" \
    --consensus-dir "$WORK/c/arti/cache/dir_blobs" \
    ${DIRCOMMON:+--dircommon "$DIRCOMMON"} \
    --phases "$PHASES" \
    --direct-phase own_device --direct-allow "$DIRECT_ALLOW" \
    --json > "$WORK/report.json"
  STATUS=$?
  set -e
  cat "$WORK/report.json"
else
  log "--no-tor: 캡처·판정 없이 시나리오만 확인했습니다"
fi
cat "$SCENARIO"
# 로그에 메시지 본문·링크가 없어야 합니다.
for secret in "$SECRET_TEXT" ${LINK:+"$LINK"} ${INVITE:+"$INVITE"} ${DEVICE_LINK:+"$DEVICE_LINK"}; do
  if grep -qF -- "$secret" "$WORK"/*/yangpad.log 2>/dev/null; then
    echo "실패: 데몬 로그에 메시지 내용·링크 중 하나가 남음" >&2
    STATUS=1
  fi
done
if [[ $EXERCISED -eq 0 ]]; then
  echo "실패: 실제 망 시나리오를 끝내지 못했습니다(캡처 판정은 위와 같음, 데몬 로그: $WORK/*/yangpad.log)" >&2
  STATUS=1
fi
exit "$STATUS"
