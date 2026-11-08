# 헤드리스 시더 (NAS·VPS)

GUI 없이 코어 데몬(`yangpad`)과 CLI(`yangpa`)만 돌려 크루의 기록·첨부를 늘 제공하는 노드를 만드는 방법입니다.
기획서의 "CLI 헤드리스 모드로 NAS나 VPS에서 상시 시더를 운영"([plan.md](plan.md) "가용성")에 해당합니다.
서버가 따로 있는 것이 아니라, 누구나 띄울 수 있는 **보통 노드 하나**를 24시간 켜 두는 것입니다.

- 아무도 제공하지 않는 데이터는 사라집니다. 상시 시더는 멤버가 모두 꺼져 있을 때도 기록과 첨부를 내줍니다.
- 앱 사용자 기기와 닿으려면 **Tor로 띄웁니다**(아래 "멤버가 닿게 하기", "Tor로 띄우기").
- 큰 음성 통화(7명 이상)의 **SFU 호스트**를 먼저 맡습니다(아래 "통화 SFU 호스트").

사용자용 설명은 [user-guide.md](user-guide.md), 로컬 API는 [api.md](api.md)를 보세요.

## 먼저 알아 둘 점

| 항목 | 내용 |
| --- | --- |
| 시더는 크루의 정식 멤버 | 시더 노드는 자기 신원으로 초대를 받아 들어가고, 멤버이므로 그 크루의 복호화 키를 가집니다. **서버를 빼앗기면 그 크루의 기록이 드러납니다.** 키 없이 암호문만 보관하려면 아래 "보관 전용 노드로 쓰기"를 보세요 |
| 디스크 키 | 서버에는 OS 키 저장소가 없어 DB 키를 데이터 경로의 0600 파일(`db.key`)에 둡니다(`--key-storage file`). DB와 키가 같은 디스크에 있으므로 디스크(또는 볼륨) 암호화를 켜 두세요 |
| 휘발성 채널 | 디스크에 쓰지 않습니다. 시더가 그 채널에 참가하더라도 내용은 RAM에만 있고, 재시작하면 사라집니다. 시더가 휘발성 기록을 보관해 주지는 않습니다 |
| 과거 기록 | 시더는 들어온 뒤의 기록을 받습니다. 과거 기록 공개가 꺼진 채널은 시더가 들어오기 전 기록을 받지도 풀지도 못하니 **크루를 만들 때 일찍** 붙이세요 |
| 로그 | 메시지 내용·키·토큰은 어떤 로그 수준에서도 남지 않습니다 |
| 원격 접근 | 데몬은 로컬 API를 네트워크에 열지 않습니다(유닉스 소켓만). 원격 관리는 SSH로 소켓을 전달합니다(아래) |
| 연결 방향 | 앱 사용자 기기는 Tor로만 돕니다. 패키지대로 직접 연결로 띄운 시더는 onion 주소에 걸 수 없고, 앱에는 시더 주소를 등록하는 설정(상시 노드)이 없어 **앱 사용자와 닿지 못합니다**. 앱 사용자의 크루에 붙이려면 시더를 Tor로 띄웁니다(아래 "멤버가 닿게 하기", "Tor로 띄우기") |

## 기본값

이 저장소의 `Dockerfile`과 `deploy/systemd/yangpad.service`가 쓰는 값입니다([decisions.md](decisions.md) "배포").

| 항목 | 값 | 이유 |
| --- | --- | --- |
| 연결 방식 | **직접 연결**(환경 변수 `YANGPA_DIRECT`) | 포트를 열고 받는 서버라서. 연결되는 상대에게 서버 IP가 보입니다. Tor로 도는 앱 사용자와는 닿지 못하므로 앱 사용자의 크루에 붙일 시더는 아래 "Tor로 띄우기"처럼 띄우세요. 데몬 자체의 기본은 Tor입니다 |
| 수신 포트 | TCP·UDP(QUIC) **47810**, IPv4만 | 공유기·방화벽에서 열 수 있게 고정(데몬 기본은 무작위 포트). Tor로 닿는 멤버는 TCP만 씁니다(QUIC은 Tor로 갈 수 없음). IPv6은 직접 더함(IPv6를 끈 커널에서도 뜨게) |
| 통화 SFU | 호스트를 먼저 맡음(`YANGPA_SFU_HOST=prefer`), 미디어 **UDP 47811**(`YANGPA_SFU_PORT`) | 늘 켜진 시더가 큰 통화를 중계하면 데스크톱의 업로드를 아낌(직접 연결일 때만). 데몬 기본은 `auto`·무작위 포트 |
| 같은 네트워크 탐색(mDNS) | 끔(`--no-mdns`) | 서버가 있는 LAN에 자신을 알릴 이유가 없음 |
| 휴면 | 끔(`YANGPA_DORMANT_AFTER=0`) | 사람이 크루를 열지 않아도 계속 구독·따라잡기 |
| DB 키 | 0600 파일(`--key-storage file`) | 서버에는 OS 키 저장소가 없음 |
| 실행 사용자 | 비루트 `yangpa`(Docker는 UID/GID 10001) | |
| 코어 덤프 | 끔 | 메모리 덤프에 키가 남지 않게 |
| 잠금 메모리 한도 | 8MiB | 휘발성 채널 키를 mlock |
| 상태 확인(Docker) | 인증 없는 `system.version`(`yangpa --json version`) | 토큰 인증 호출은 감사 로그에 남아, 주기 확인이 기록을 밀어내지 않게 |

## 방법 1: Docker / Compose

```bash
# 저장소 루트에서 이미지 빌드(처음에는 Rust 전체 빌드라 오래 걸립니다)
docker build -t yangpa-seeder .

# 또는 예시 compose 파일로 빌드와 실행을 한 번에
docker compose -f deploy/docker-compose.yml up -d --build
docker compose -f deploy/docker-compose.yml ps        # STATUS가 (healthy)이면 정상
docker compose -f deploy/docker-compose.yml logs -f
```

릴리스에 올라간 이미지 파일(`yangpa-seeder-<판>-linux-amd64.tar.gz`, ARM NAS·라즈베리 파이 같은 aarch64 기기는
`…-linux-arm64.tar.gz`)을 쓰려면 `docker load < 파일` 뒤 compose 파일의 `image:`를 그 이름(`yangpa-seeder:<판>`)으로
바꾸고 `build:`를 지우세요. 두 이미지 모두 릴리스 때 그 아키텍처 러너에서 빌드해 같은 연기 시험(건강 검사·사용자·권한·
정상 종료)을 거칩니다. 기기에서 직접 빌드해도 같은 Dockerfile이 그대로 됩니다.

- 데이터는 볼륨 `/var/lib/yangpa`(0700)에만 씁니다: SQLCipher DB, `db.key`, 블롭, 소유자 토큰(`owner.token`),
  Arti(Tor) 캐시, 로컬 API 소켓(`yangpad.sock`, 0600). 루트 파일 시스템은 읽기 전용으로 둘 수 있습니다.
- 볼륨 대신 호스트 디렉터리를 쓰려면 미리 `install -d -m 0700 -o 10001 -g 10001 /srv/yangpa`처럼 만들어 두세요.
  데몬은 이미 있는 디렉터리의 권한을 바꾸지 않고, Arti(Tor)는 남이 쓸 수 있는 디렉터리를 거부합니다.
  NFS·SMB 공유 폴더는 유닉스 소켓을 만들 수 없으니 쓰지 마세요.
- CLI는 컨테이너 안에서 씁니다. 이미지가 데이터 경로·엔드포인트 환경 변수를 넣어 두었고, 같은 사용자라 소유자
  토큰을 바로 읽습니다.

  ```bash
  docker exec -it yangpa-seeder yangpa status
  ```

- 인자를 바꾸려면 compose의 `command:`에 **전체 인자**를 다시 적습니다(기본:
  `--no-mdns --listen /ip4/0.0.0.0/tcp/47810 --listen /ip4/0.0.0.0/udp/47810/quic-v1`). `--key-storage file`은
  `ENTRYPOINT`에 있어 늘 붙습니다. 그 밖의 인자는 `docker run --rm yangpa-seeder --help`.
- IPv6나 같은 네트워크 탐색이 필요하면 `network_mode: host`로 바꾸고 `--listen /ip6/::/tcp/47810` 등을 더하세요.

## 방법 2: systemd

릴리스의 `yangpa-cli-<판>-x86_64-unknown-linux-gnu.tar.gz`(ARM64 기기는 `…-aarch64-unknown-linux-gnu.tar.gz`,
둘 다 glibc 2.35 이상)를 받거나 소스에서 빌드합니다(`cargo build --release -p yangpa-daemon -p yangpa-cli`).

```bash
# 받은 파일 확인(같은 릴리스의 SHA256SUMS.txt와 함께)
sha256sum -c SHA256SUMS.txt --ignore-missing
tar xzf yangpa-cli-*-"$(uname -m)"-unknown-linux-gnu.tar.gz && cd yangpa-cli-*/

sudo install -m 0755 yangpad yangpa /usr/local/bin/
sudo install -m 0644 deploy/systemd/yangpa.sysusers.conf /etc/sysusers.d/yangpa.conf
sudo systemd-sysusers                      # 'yangpa' 시스템 사용자(로그인 셸 없음)
sudo install -m 0644 deploy/systemd/yangpad.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now yangpad
systemctl status yangpad
journalctl -u yangpad -f
```

- 데이터는 `/var/lib/yangpa`(0700, `StateDirectory`), 로컬 API 소켓은 `/run/yangpa/yangpad.sock`(0700 디렉터리,
  `RuntimeDirectory`라 서비스를 멈추면 사라짐)입니다.
- 단위는 루트 권한·능력(capability) 없이, 쓸 수 있는 경로를 이 두 곳으로 막고, 커널·장치·네임스페이스 접근과
  시스템 호출을 줄여 실행합니다(`systemd-analyze security yangpad`로 확인).
- 인자는 단위 파일을 고치지 말고 `sudo systemctl edit yangpad`로 덮어쓰세요. `ExecStart`는 먼저 비워야 합니다.

  ```ini
  [Service]
  ExecStart=
  ExecStart=/usr/local/bin/yangpad --key-storage file --no-mdns --relay-server \
      --listen /ip4/0.0.0.0/tcp/47810 --listen /ip4/0.0.0.0/udp/47810/quic-v1
  ```

- **IPv6.** 기본 단위는 IPv4(`/ip4/0.0.0.0`)만 엽니다. 데몬은 수신 주소 중 하나라도 열지 못하면 시작하지 않는데,
  IPv6를 끈 커널(`ipv6.disable=1`)에서는 IPv6 소켓을 만들 수 없어 `Restart=on-failure`로 다시 시작만 되풀이하기
  때문입니다. IPv6 주소가 있는 서버는 드롭인으로 더하세요(`--relay-server` 등 다른 인자도 같은 줄에).

  ```ini
  [Service]
  ExecStart=
  ExecStart=/usr/local/bin/yangpad --key-storage file --no-mdns \
      --listen /ip4/0.0.0.0/tcp/47810 --listen /ip4/0.0.0.0/udp/47810/quic-v1 \
      --listen /ip6/::/tcp/47810 --listen /ip6/::/udp/47810/quic-v1
  ```

  서비스가 바로 끝나며 다시 시작을 되풀이하면 `journalctl -u yangpad`에서 "노드를 시작할 수 없습니다" 오류를 보세요.
  `Address family not supported`면 IPv6 `--listen`을, `Address in use`면 같은 포트를 쓰는 다른 프로그램을 확인합니다.
- 서버에서 CLI를 쓸 때는 서비스 사용자로 실행합니다(소켓·소유자 토큰이 그 사용자 것). 별칭을 두면 편합니다.

  ```bash
  alias yp='sudo -u yangpa env YANGPA_DATA_DIR=/var/lib/yangpa YANGPA_ENDPOINT=/run/yangpa/yangpad.sock yangpa'
  yp status
  ```

`yangpa daemon [인자…]`도 같은 데몬을 띄웁니다(같은 디렉터리나 `PATH`의 `yangpad`를 찾아 인자를 그대로 넘김).
상시 운영에는 위처럼 서비스 관리자에 맡기세요.

## 처음 설정

아래 예시의 `yangpa`는 Docker면 `docker exec -it yangpa-seeder yangpa`, systemd면 위의 `yp`입니다.

1. **신원 만들기.** 시더 신원을 잃으면 새로 만들어 다시 초대받습니다. 같은 신원을 다른 서버로 옮기려면 데이터 경로를
   통째로 옮깁니다(아래 "백업").

   ```bash
   yangpa identity create --name "게임 모임 시더"
   ```

2. **크루에 들어가기.** 크루에서 초대 권한이 있는 사람이 초대 링크를 만들어 주면(`yangpa invite create --crew …`
   또는 앱) 시더가 그 링크로 들어갑니다. 링크에 실린 것은 onion 주소이므로 시더가 **Tor로 떠 있어야** 들어갈 수 있습니다
   (아래 "멤버가 닿게 하기"). 관리자는 필요하면 시더에게 역할을 줍니다.

   ```bash
   yangpa invite join 'yangpa://invite/…'
   yangpa space list
   ```

3. **시더 설정.** 기본 시더 범위는 "본인이 연 채널만"이라 아무 채널도 열지 않는 헤드리스 노드는 거의 제공하지
   않습니다. **반드시 범위를 넓히세요.** `--crew`를 빼면 모든 공간의 기본값이 바뀝니다.

   ```bash
   yangpa seed set --all-channels --types text,image,video,file,emoji,history_bundle --upload-limit 5MB/s --max-concurrent 8
   yangpa seed show
   ```

4. **받을 데이터.** 시더는 자기가 가진 데이터만 제공합니다. 첨부를 자동으로 받을 크기 한도를 올리세요(기본 20MiB.
   첨부 하나의 크기는 기본 무제한이고 크루 관리자가 채널마다 한도를 둘 수 있으며, 채널 한도를 넘는 첨부는 자동으로 받지 않음).

   ```bash
   yangpa peer set --types text,image,video,file,emoji,history_bundle --auto-download-max 100MB
   ```

5. **미리 받기와 저장 공간(첨부).** 보통 노드는 열어 보지 않은 채널의 첨부를 그 채널을 열 때까지 받지 않습니다
   ([decisions.md](decisions.md) "자동 내려받기"). 헤드리스 노드는 **미리 받기**를 켜세요. 켜면 볼 수 있는 모든
   채널(휘발성 채널 제외)의 첨부를 열지 않아도 받아 두고 내줍니다. 새 메시지(10분 안에 쓴 것)의 첨부는 바로, 지난
   메시지의 첨부(꺼져 있는 동안 쌓여 따라잡은 것 포함)는 1분마다 최근 것부터 남은 자리만큼 받습니다. 종류·크기는 4단계의
   피어 설정을 따릅니다.

   ```bash
   yangpa seed set --all-channels --prefetch on
   ```

   받은 첨부가 쓰는 디스크는 **저장 공간 한도**(기본 2GiB)와 **최소 여유 공간**(기본 1GiB) 안으로 묶입니다. 넘으면
   오래 안 쓴 첨부부터 데이터를 비우고(다시 받을 수 있음), 미리 받기는 남은 자리만 씁니다. 상시 시더는 디스크에 맞게
   올리세요. 값은 저장되어 재시작해도 남습니다. AI(자동화 토큰)가 한도를 줄이거나 최소 여유를 늘리면 받은 첨부가 비워질 수
   있어 사람 확인을 받습니다.

   ```bash
   yangpa blob cache limit 200GB --min-free 10GB
   yangpa blob cache status          # 정리 대상·고정·늘 남기는 것, 한도, 디스크 여유
   ```

   처음부터 정해 두려면 데몬 인자 `--cache-limit 200GB --cache-min-free 10GB`(환경 변수 `YANGPA_CACHE_LIMIT`,
   `YANGPA_CACHE_MIN_FREE`)를 쓰세요. 이것은 기본값이라 `blob cache limit`으로 저장한 값이 우선하고, `--reset`으로
   기본값에 돌아갑니다. 크루 이모지·아이콘·프로필 이미지와 이 노드가 올린 첨부는 한도와 무관하게 남습니다. 꼭 남길
   첨부는 `yangpa blob pin --crew "게임 모임" <블롭 ID>`로 고정합니다.

6. **확인.** 채널마다 지금 기록을 내줄 수 있는 온라인 시더 수가 나옵니다. 0이면 아무도 제공하지 않는 채널입니다.

   ```bash
   yangpa seed coverage --crew "게임 모임"
   yangpa net status
   ```

### 내 기기로 붙이기(선택)

1대1 스페이스는 멤버가 두 명뿐이라 별도 신원의 시더가 들어갈 수 없습니다. 내 대화를 서버가 제공하게 하려면 서버를
**내 기기 하나로** 들입니다. 쓰던 기기가 띄운 일회용 링크로 서버가 연결을 청하고, 쓰던 기기에서 번호를 맞춰 보고
허용합니다. 서버에는 신원을 만들지 않은 채로 두고, 쓰던 기기가 Tor로 돌면 서버도 Tor로 띄웁니다.

```bash
# 쓰던 기기에서(앱은 설정 › 기기 › 새 기기 연결의 '링크 복사')
yangpa device link                       # 링크를 출력(10분 안에, 한 기기만)

# 서버에서
yangpa device join                       # 화면에 보이지 않는 입력 줄에 링크를 넣고 Enter. 6자리 번호를 출력
```

링크는 인자로 받지 않아 셸 기록에 남지 않습니다. 쓰던 기기에 뜬 번호가 서버의 번호와 같을 때만 허용하세요.

```bash
# 쓰던 기기에서(앱은 허용 창이 뜸)
yangpa device link-pending               # 기다리는 기기와 번호
yangpa device link-approve               # 번호가 다르면 link-reject
yangpa device list                       # 서버 기기가 보이면 들어온 것
```

허용하면 서버가 신원을 받고 내 다른 기기에게서 공간을 받아 합류합니다(친구·크루 멤버에게는 경고 없이 새 기기 하나로
보입니다). 도중에 그만두려면 서버에서 `yangpa device join-abort`, 쓰던 기기에서 `yangpa device link-cancel`을 씁니다.

대가: 내 **신원 키**가 서버에 놓입니다. 서버를 잃으면 다른 기기에서 `yangpa device revoke <기기 ID>`로 그 기기를
폐기하세요. 폐기한 기기는 이후 그룹 키를 받지 못하지만, 이미 받은 기록과 신원 키 자체는 되돌릴 수 없습니다(신원 키가
샌 것으로 보고 새 신원을 만드는 것이 안전합니다).

## 보관 전용 노드로 쓰기

시더와 달리 **크루 멤버가 아니고 복호화 키를 받지 않는** 노드입니다(기획서의 선택 기능 "보관 전용 노드",
[protocol.md](protocol.md) "보관 전용 노드"). 크루 관리자가 지정하면 멤버들에게서 암호화된 기록·첨부를 받아 두었다가,
멤버가 모두 꺼져 있어도 **지금 멤버인 기기에게만** 내줍니다. 서버를 빼앗겨도 내용은 드러나지 않습니다.

| | 시더(멤버) | 보관 전용 노드 |
| --- | --- | --- |
| 크루 멤버 | 예(초대로 들어감) | 아니요(관리자가 지정) |
| 내용·키 | 가짐 | 없음(암호문·메타데이터만) |
| 내주는 상대 | 시더 설정대로 | 명단의 지금 멤버 기기만(추방·밴·폐기 기기는 거절) |
| 비공개 채널 | 볼 수 있는 채널 | 지정할 때 고른 채널만 |
| 휘발성 채널 | 디스크에 안 씀 | 받지 않음 |

보관 노드가 볼 수 있는 것: 멤버 사용자·기기 ID와 들어온 시점, 누가 언제 어느 채널에 얼마만 한 메시지를 썼는지(이벤트 헤더),
첨부 크기, 접속한 기기의 주소. 크루·채널 이름과 메시지·첨부 내용은 볼 수 없습니다.

1. **보관 모드로 띄우기.** 데몬 인자에 `--archive`를 더하거나 환경 변수 `YANGPA_ARCHIVE=1`을 둡니다. 공간마다 받아 둘
   용량 상한은 `--archive-quota <바이트>`(`YANGPA_ARCHIVE_QUOTA`, 기본 20GiB)입니다. 모든 이벤트(헤더·본문)와 첨부를 세며,
   메시지 본문·첨부는 상한의 7/8까지만 받고(넘으면 헤더만 받아 두고 내주지 않음) 나머지는 상태·키 이벤트 몫으로 남깁니다. 시더와 같은 서버에서 같이
   돌리지 말고 **신원을 따로** 만드세요(멤버의 노드는 보관 노드로 지정할 수 없음).

   ```bash
   yangpad --key-storage file --no-mdns --direct --archive --listen /ip4/0.0.0.0/tcp/47810
   yangpa identity create --name "동아리 보관함"
   ```

2. **지정 링크 만들기(보관 노드에서).** 1회용이고 기본 24시간 뒤 만료됩니다. 크루 관리자에게만 따로 전하세요.

   ```bash
   yangpa archive offer --name "동아리 보관함" --expires 24h
   ```

3. **지정하기(크루 관리자의 기기에서).** 크루 관리 권한이 있어야 합니다. 공개 채널은 늘 보관하고, 비공개 채널은
   `--private-channels`로 고른 것만 보관합니다. 앱에서는 크루 설정 → 시더 → 보관 전용 노드에서 합니다.

   ```bash
   yangpa archive designate --crew "게임 모임" 'yangpa://archive/…'
   yangpa archive designate --crew "게임 모임" 'yangpa://archive/…' --private-channels 운영
   yangpa archive list --crew "게임 모임"
   ```

4. **확인.** 보관 노드에서 받아 둔 크루를 수치로 봅니다. 멤버 쪽 `seed coverage`에는 "온라인 보관 노드" 열이 따로 나옵니다.

   ```bash
   yangpa archive hosted
   yangpa seed coverage --crew "게임 모임"
   ```

5. **해제.** 크루 관리자가 해제하면 보관 노드는 그 크루 데이터를 지우고 더 받지도 내주지도 않습니다. 보관 노드 운영자가
   `archive drop`으로 스스로 지울 수도 있습니다. 이때는 크루에 알리지 않으므로 멤버 쪽 지정은 남고(멤버는 닿지 않는 보관
   노드로 보고 물러나며 다시 시도), 다시 받아 두려면 새 지정 링크로 다시 지정해야 합니다.

   ```bash
   yangpa archive revoke --crew "게임 모임" "동아리 보관함"
   yangpa archive drop <공간 ID>
   ```

- 채팅 수명이 있는 채널은 작성 시각 기준으로 수명이 지나면 본문을 지웁니다. 지정에서 빠진 채널은 바로 지우지 않고, 더 새로운
  명단도 빠졌다고 하거나 72시간이 지나면 지웁니다(따라잡지 못한 관리자 기기의 명단 때문에 기록을 잃지 않게).
- 보관 노드는 멤버의 시더 설정을 따릅니다. 멤버가 시딩을 끄거나 어떤 채널을 빼면 그 멤버에게서는 받지 않습니다.
- 새 멤버가 크루에 들어가는 것(가입 기기를 넣은 크루 키 갱신)은 확정 기기가 한 번은 온라인이어야 합니다. 그 뒤의 기록·첨부는 보관 노드만
  켜져 있어도 받습니다. 과거 기록이 비공개인 채널은 들어오기 전 기록을 보관 노드도 내주지 않습니다.
- 보관 노드의 접속 정보에는 서버 IP가 들어가지 않습니다. 직접 연결로 띄운 보관 노드는 멤버의 onion 주소로 걸 수 없고
  멤버도 그 주소를 등록할 곳이 없으니, 아래 "Tor로 띄우기"처럼 Tor로 띄우세요.

## 원격 관리 (토큰 + SSH 소켓 전달)

데몬은 로컬 API를 TCP로 열지 않습니다(기획서 "원격 접근은 기본 비활성"). 소켓은 0600이라 서비스 사용자만 붙을 수
있습니다. 다른 컴퓨터에서 관리하려면 **SSH로 소켓을 전달**하고, 소유자 토큰 대신 **범위를 좁힌 API 토큰**을 씁니다.
두 가지가 다 있어야 하므로 SSH 키가 새도 토큰 범위 밖의 일(신원·토큰 발급 등)은 할 수 없습니다.

1. **서버에서 토큰 발급**(소유자 토큰이 필요하므로 서버에서). 토큰 원문은 이때 한 번만 나옵니다.

   ```bash
   # 시더 설정·상태만 다루는 토큰. 특정 크루로 좁히려면 --crews "게임 모임"
   yangpa token create --name 원격관리 --scopes read,settings
   yangpa token list
   ```

   | 범위 | 할 수 있는 일 |
   | --- | --- |
   | `read` | 상태·공간·채널·메시지 읽기 |
   | `write` | 메시지 보내기 등 |
   | `admin` | 역할·추방·초대·채널 관리 |
   | `settings` | 시더·피어 설정·토큰 목록·폐기·감사 로그(STUN 서버 목록은 사람만 바꿈) |
   | `owner` | 전부(원격용으로 발급하지 마세요) |

2. **서버의 SSH에서 `yangpa` 사용자에게 소켓 전달만 허용**합니다(`/etc/ssh/sshd_config` 끝에 더하고 `sshd -t` 뒤
   다시 읽기). 관리할 사람의 공개키를 `/etc/ssh/yangpa_authorized_keys`에 넣습니다.

   ```text
   Match User yangpa
       AuthorizedKeysFile /etc/ssh/yangpa_authorized_keys
       AllowStreamLocalForwarding local
       AllowTcpForwarding no
       X11Forwarding no
       AllowAgentForwarding no
       PermitTTY no
       ForceCommand /usr/sbin/nologin
   ```

   Docker로 돌린다면 볼륨의 소켓 경로(예: 호스트 디렉터리 `/srv/yangpa/yangpad.sock`)를 쓰고, 그 소켓 소유자(UID
   10001)로 로그인하는 사용자를 따로 두거나 서버에 SSH로 들어가 `docker exec`로 관리하세요.

3. **관리하는 컴퓨터에서** 소켓을 전달해 두고 CLI를 그 소켓으로 씁니다(리눅스·macOS). 전달된 소켓은 0600으로 생깁니다.

   ```bash
   mkdir -p ~/.yangpa && chmod 700 ~/.yangpa
   ssh -N -o StreamLocalBindUnlink=yes \
       -L ~/.yangpa/seeder.sock:/run/yangpa/yangpad.sock yangpa@seeder.example.org &

   export YANGPA_TOKEN='발급받은 토큰'        # 명령줄 인자는 다른 사용자에게 보일 수 있어 환경 변수로
   yangpa --endpoint ~/.yangpa/seeder.sock status
   yangpa --endpoint ~/.yangpa/seeder.sock seed set --crew "게임 모임" --upload-limit 10MB/s
   ```

   발급받은 토큰(`owner` 범위가 아닌 것)으로는 파일 경로를 넘기는 명령(첨부 보내기, `blob save`, 이모지·프로필 이미지)을 쓸 수
   없습니다. 경로는 서버의 파일을 가리켜, 토큰을 가진 쪽이 서버의 아무 파일(소유자 토큰 포함)이나 읽게 되기 때문입니다.

4. **기록과 폐기.** 인증한 모든 호출은 감사 로그에 남습니다(메서드·토큰 이름·결과·시각, 인자와 내용은 남지 않음).
   필요 없어진 토큰은 바로 폐기하세요. 이미 붙어 있는 연결의 다음 호출도 막힙니다.

   ```bash
   yangpa audit list -n 50
   yangpa token revoke <토큰 ID>
   ```

AI 도구(MCP)를 서버에 붙이는 것은 권하지 않습니다. 붙인다면 `--automation`으로 발급한 자동화 토큰만 쓰세요
(파괴적 작업은 사람 확인을 거침, [user-guide.md](user-guide.md) "AI 연동").

## 멤버가 닿게 하기

앱 사용자 기기는 Tor로만 돌고, 직접 연결로 띄운 시더는 Tor를 쓰지 않아 onion 주소에 걸 수 없습니다. 예전에는 멤버가
시더의 TCP 주소를 상시 노드로 등록해 먼저 걸었지만, 이 설정은 앱·CLI·로컬 API에서 빠졌습니다([decisions.md](decisions.md),
2026-10-22). 그래서 **앱 사용자의 크루에 붙일 시더는 Tor로 띄웁니다**(아래 "Tor로 띄우기"). 그러면 초대 링크로 들어가기와
멤버와 닿기가 앱 사용자 기기와 같고, 서버 IP도 드러나지 않습니다.

직접 연결 시더는 같은 방식으로 띄운 다른 직접 연결 노드(다른 시더·시험 노드)끼리만 닿습니다. 시더 인자에
`--relay-server`를 더하면 그런 노드의 연결도 중계합니다(내용은 종단간 암호화라 볼 수 없음, 릴레이 예약은 외부 주소가
확인된 뒤에 받음). 앱 사용자의 Tor 연결은 릴레이를 쓰지 않습니다.

## 통화 SFU 호스트

음성 통화 참가자가 6명을 넘으면 한 노드가 참가자마다 연결 하나씩을 받아 소리·화면 프레임을 **풀지 않고** 나눠
보냅니다(SFU). 시더 패키지는 이 역할을 먼저 맡겠다고 알립니다(`YANGPA_SFU_HOST=prefer`). 시더가 통화에 참가하지 않아도
그 통화 채널의 통화가 커지면 호스트가 됩니다([protocol.md](protocol.md) "SFU").

- **볼 수 있는 것.** 프레임은 참가자 앱이 이미 암호화했고 시더는 프레임 키를 받지 않으므로 소리·화면 내용은 볼 수
  없습니다. 그래도 모든 참가자의 IP, 누가 언제 말하는지(소리 패킷의 크기·시각), 트랙마다
  비트율, 누가 어떤 트랙을 보내는지를 압니다. 메시에서는 참가자끼리 서로만 아는 것을 한 노드가 다 보므로 관리자가 맡길
  노드를 정합니다(아래 "자격"). 참가자 앱에는 누구의 노드가 중계하는지 보입니다("SFU · 중계: ○○의 통화 밖 노드").
- **자격.** 시더 계정에 그 통화 채널의 **보기(`view_channel`)·음성 연결(`connect_voice`)·음성 관리(`manage_voice`)**
  권한이 있어야 합니다. 보기 권한이 없으면 통화 소식을 받지 못하고, 연결·음성 관리 권한이 없으면 시더가 나서지 않으며
  나서도 참가자가 받아들이지 않습니다. `manage_voice`는 크루의 기본(@everyone) 권한에 없으므로 시더 전용 역할로 줍니다.

  ```bash
  yangpa role create --crew "게임 모임" --name 시더 --perms manage_voice
  yangpa member roles --crew "게임 모임" 시더계정 --roles 시더
  ```

  진행 중인 통화에 나중에 권한을 주면 20초 안에 그 통화에도 나섭니다. 권한을 빼면 후보에서 물러나고 다음 후보가 이어
  맡습니다.
- **포트.** 미디어는 UDP 47811 하나로 받습니다(P2P 포트 47810과 따로). 공유기·방화벽에서 UDP 47811을 열어 주세요.
  Docker는 compose의 `47811:47811/udp`, systemd는 방화벽만 열면 됩니다.
- **바깥 주소.** 참가자 앱은 SDP에 실린 호스트 후보로 붙습니다. 공인 IP가 인터페이스에 바로 있는 VPS(systemd)는 그대로
  됩니다. Docker 브리지 네트워크나 공유기 NAT 뒤라면 시더는 자기 사설 IP만 알므로 공인 IP를 알려 주세요.

  ```bash
  # Docker compose의 environment:
  YANGPA_SFU_ANNOUNCE: "203.0.113.7"
  # systemd 드롭인(sudo systemctl edit yangpad):
  [Service]
  Environment=YANGPA_SFU_ANNOUNCE=203.0.113.7
  ```

  데몬의 STUN 서버(기본은 Google·Cloudflare 공개 STUN)로 알아낸 바깥 주소도 함께 씁니다. 목록은 `yangpa net stun`으로
  보고 `yangpa net stun set`·`reset`·`off`로 바꿉니다(사람만, 로컬 API `net.set_stun_servers`).

  ```bash
  yangpa net stun set stun.example.org:3478 198.51.100.7
  yangpa net stun off        # STUN 쓰지 않기(공유기 밖 주소는 YANGPA_SFU_ANNOUNCE로만)
  ```

- **수용 인원.** 기본 25명(`YANGPA_SFU_CAPACITY` 또는 `--sfu-capacity`). 여러 통화를 맡으면 모두 합쳐 이 수까지라,
  다른 통화에는 남은 자리만 알립니다(그 자리보다 큰 통화에서는 이 시더를 고르지 않음). 맡던 통화가 끝나면 늘어난 자리를
  다시 알려 기다리던 통화를 넘겨받습니다. 회선이 좋으면 늘리세요(화면 하나를 볼 때 시청자마다 수백 kbps~수 Mbps를 내보냄).
- **맡지 않게 하기.** `YANGPA_SFU_HOST=never`(또는 `--sfu-host never`). `auto`는 시더가 직접 참가한 통화에서만 후보가
  됩니다. **Tor로 띄우면 설정과 상관없이 맡지 않습니다**(호스트 주소가 실제 IP라 참가자에게 드러남).
- **소켓을 열 주소.** 기본은 노드 수신 주소의 IP입니다. 여러 인터페이스 중 하나만 쓰려면 `YANGPA_SFU_BIND`(쉼표로
  여러 개). 소켓을 열지 못하면 로그에 "SFU 미디어 소켓을 하나도 열지 못해 호스트를 맡지 않음"이 남고 60초 동안 후보로
  나서지 않습니다.
- **넘겨주기.** 시더를 멈추거나 다시 시작하면 그동안 다른 후보(회선이 좋은 참가자)가 이어 맡고, 참가자 앱은 새 호스트에
  다시 붙습니다(잠깐 끊김). 시더가 돌아와 다시 후보로 나서면(새 참가 알림을 받거나 20초마다 다시 볼 때) 시더가 도로
  맡습니다. 그 밖에는 지금 호스트가 계속 맡을 수 있는 동안 더 나은 후보가 와도 바꾸지 않습니다.
- **확인.** `yangpa net status`로 노드가 공인 주소로 닿는지(`NAT: 공인`) 봅니다. 공인으로 확인됐거나
  `YANGPA_SFU_ANNOUNCE`가 있으면 호스트 순위에서 앞섭니다. 로그 수준을 `yangpa_daemon=debug`로 올리면 "SFU 세션 시작"·
  "통화 토폴로지 결정" 같은 줄이 보입니다(SDP·후보·IP는 어떤 수준에서도 남기지 않음).

## Tor로 띄우기

서버의 IP도 숨기려면 시더를 앱 사용자 기기처럼 Tor로 띄웁니다. 데몬 안의 Tor 클라이언트(Arti)를 쓰므로 Tor를 따로 설치할
필요가 없습니다. 패키지는 직접 연결로 시작하므로(위 "기본값") 그 설정을 끕니다.

- Docker: compose의 `environment:`에 `YANGPA_DIRECT: "false"`를 넣습니다(들어오는 포트가 필요 없어 `ports` 항목을 지워도 됨).
- systemd: `sudo systemctl edit yangpad`로 `[Service]` 아래에 `Environment=YANGPA_DIRECT=false`를 넣습니다.
- 손으로 띄울 때는 `--direct`를 빼면 됩니다(기본이 Tor).

```bash
yangpa net status        # mode가 tor인지, onion_addr, bootstrap_progress
```

- 나가는 연결은 모두 Tor로, 들어오는 연결은 onion 서비스로 받습니다. 공유기 포트를 열 필요가 없고, 초대 링크로 들어가기와
  멤버와 닿기가 앱 사용자 기기와 같습니다. STUN 서버에도 묻지 않습니다.
- 통화 SFU 호스트는 맡지 않습니다(위 "통화 SFU 호스트").
- 끄는 것: identify 주소 광고, DHT에 IP 등록(DHT는 클라이언트로만), mDNS, AutoNAT, 홀펀칭, **릴레이(서버·클라이언트)**,
  링크 미리보기 직접 조회. 그래서 Tor로 띄운 시더는 다른 노드의 릴레이가 될 수 없습니다.
- 대가: 지연이 크게 늘고(목표 p95 3초), 파일은 Tor 회로가 먼저 한계라 보통 초당 수십~수백 KiB입니다. 기본값도 동시에 보내 줄 기기 2대,
  자동 내려받기 2MiB로 줄어듭니다(대역폭 상한은 직접 연결과 같은 10MiB/s). 시더라면 `seed set --upload-limit`·
  `peer set --auto-download-max`로 직접 정하세요(저장한 값이 기본값보다 우선).
- 첫 시작 때 Tor 망 정보를 받느라 시간이 걸립니다. 캐시는 데이터 경로(`arti/`)에 남아 다음부터 빠릅니다.
- Tor가 막힌 곳에서는 브리지를 씁니다([protocol.md](protocol.md) "Tor 브리지"). 브리지 전송 프로그램 `lyrebird`를 PATH(또는 `yangpad`
  옆)에 두고 로컬 API `net.set_bridges`로 켭니다(사람 토큰, 명령줄 도구에는 아직 명령이 없음). lyrebird가 있고 브리지 설정을 한 번도 저장하지 않았으면, Tor에 들어가지 못할 때 내장
  Snowflake를 스스로 켭니다.
- **Tor를 준비할 수 없으면 데몬이 시작하지 않습니다**(직접 연결로 조용히 떠서 IP가 드러나지 않게). 로그를 보세요. 실행 중에
  연결 방식을 바꾸는 명령은 없습니다(설정을 고쳐 다시 시작).
- 네트워크 키(PeerId)는 직접 연결과 Tor가 따로입니다. 직접 연결로 쓰던 시더를 Tor로 바꾸면 Tor 키로 시작하고 예전 직접 연결
  키는 지웁니다. 친구·멤버는 암호화 채널 안에서 신원 키로 같은 기기임을 확인합니다.

## 운영

| 할 일 | 방법 |
| --- | --- |
| 상태 | `yangpa status`, `yangpa net status`, `yangpa net peers`, Docker는 `docker ps`의 `(healthy)` |
| 제공 현황 | `yangpa seed coverage --crew …`(0인 채널은 경고 대상) |
| 자원 | `yangpa diag metrics`(지연·메모리·CPU·대기열). 유휴 목표는 메모리 150MB 이하, CPU 1% 미만 |
| 저장 공간 | `yangpa blob cache status`(받은 첨부 사용량·한도·디스크 여유·정리한 양), `blob cache limit`, `blob pins` |
| 휘발성 누출 검사 | 휘발성 채널에 표식 문자열을 보낸 뒤 `yangpa diag leakcheck <표식>`(데이터 경로·로그 파일을 훑음) |
| 로그 | `journalctl -u yangpad` / `docker logs yangpa-seeder`. 수준은 `YANGPA_LOG`(예: `info`, `yangpa_daemon=debug`) |
| 종료 | `systemctl stop yangpad` / `docker compose stop`(SIGTERM). 네트워크를 닫고 휘발성 상태를 지운 뒤 DB를 정리합니다 |
| 업데이트 | 새 바이너리로 바꾸고 다시 시작 / `docker compose up -d --build`. 출시 전이라 판 사이 저장 형식이 호환되지 않을 수 있으니 릴리스 노트를 보세요 |

### 백업

- 데이터 경로 전체(볼륨)가 시더의 모든 것입니다. **DB 키(`db.key`)와 소유자 토큰이 함께 들어 있어 백업본을 읽을 수
  있는 사람은 크루의 기록을 모두 풀 수 있습니다.** 그래서 아래 명령은 백업 파일을 처음부터 0600으로 만듭니다
  (`umask 077`. 이것이 없으면 기본값 0644로 생겨 같은 서버의 다른 사용자가 읽을 수 있음). 일관된 사본을 위해 서비스를
  멈춘 뒤 복사하고, 백업 파일은 남이 들어올 수 없는 0700 디렉터리에 둡니다.
- Docker(저장소 루트에서):

  ```bash
  install -d -m 0700 "$HOME/yangpa-backup"
  docker compose -f deploy/docker-compose.yml stop
  docker run --rm -v yangpa_yangpa-data:/data -v "$HOME/yangpa-backup":/backup debian:trixie-slim \
      sh -c 'umask 077 && tar czf /backup/yangpa-data.tar.gz -C /data .'
  docker compose -f deploy/docker-compose.yml start
  # 컨테이너가 root로 만든 파일을 내 것으로(권한은 0600 그대로)
  sudo chown "$(id -u):$(id -g)" "$HOME/yangpa-backup/yangpa-data.tar.gz"
  ```

- systemd:

  ```bash
  install -d -m 0700 "$HOME/yangpa-backup"
  sudo systemctl stop yangpad
  sudo sh -c 'umask 077 && tar czf "$1" -C /var/lib/yangpa .' sh "$HOME/yangpa-backup/yangpa-data.tar.gz"
  sudo systemctl start yangpad
  sudo chown "$(id -u):$(id -g)" "$HOME/yangpa-backup/yangpa-data.tar.gz"
  ```

- 서버 밖(다른 디스크·클라우드)으로 옮길 때는 암호로 한 번 더 감싸세요. 예: `age -p -o yangpa-data.tar.gz.age
  yangpa-data.tar.gz` 뒤 원본을 지움. 되살릴 때는 같은 경로에 풀고 소유자(Docker는 10001, systemd는 `yangpa`)와
  0700을 맞춥니다.

## 문제 해결

| 증상 | 확인할 것 |
| --- | --- |
| `데몬에 연결할 수 없음`(종료 코드 4) | 데몬이 떠 있는지, 엔드포인트가 맞는지(`YANGPA_ENDPOINT`), 같은 사용자인지(소켓 0600. 루트로 실행해도 소켓 서버가 다른 사용자면 토큰을 보내지 않고 끊음) |
| `토큰이 없거나 잘못됨` | 서비스 사용자로 실행했는지(소유자 토큰은 데이터 경로), 원격이면 `YANGPA_TOKEN` |
| `이 토큰으로는 할 수 없는 작업`(1002) | 토큰 범위. 시더 설정은 `settings`, 토큰 발급은 서버의 소유자 토큰 |
| 아무와도 연결되지 않음 | 직접 연결로 띄웠다면 Tor로 도는 앱 사용자와는 닿지 못합니다("멤버가 닿게 하기"). Tor로 띄웠다면 `yangpa net status`의 `bootstrap_progress`, 직접 연결 노드끼리라면 포트 47810 TCP·UDP와 외부 주소·NAT 상태 |
| 데몬이 `invalid value … for '--direct'`로 바로 끝남 | `YANGPA_DIRECT`는 `true`·`false`만 받습니다 |
| 첨부를 제공하지 않음 | `seed set --all-channels --prefetch on`, `peer set --auto-download-max`, `blob cache status`의 한도·디스크 여유(위 5단계) |
| 데이터 경로 권한 오류 | 바인드 마운트 디렉터리 소유자(10001)와 0700 |
| Tor로 띄울 때 시작 실패 | 로그의 Tor 오류(데이터 경로 `arti/` 권한 등). 직접 연결로는 넘어가지 않습니다. 시작은 했는데 `bootstrap_progress`가 오르지 않으면 바깥으로 나가는 TCP가 막혔는지 |
