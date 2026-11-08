# 로컬 API (JSON-RPC 2.0)

GUI·CLI·MCP 서버가 모두 이 API를 씁니다. 원격 접근은 기본 비활성입니다.

## 전송

- 엔드포인트: Unix `$XDG_RUNTIME_DIR/yangpa/yangpad.sock`(없으면 `<데이터 경로>/yangpad.sock`, 권한 0600),
  Windows named pipe `\\.\pipe\yangpad-<사용자 이름>`. `--endpoint`/`YANGPA_ENDPOINT`로 바꿀 수 있습니다.
- Windows pipe 이름은 남이 짐작해 먼저 만들 수 있으므로, 클라이언트는 연결 직후 **토큰을 보내기 전에** pipe 서버
  프로세스(`GetNamedPipeServerProcessId`)가 자기와 같은 OS 사용자(프로세스 토큰의 사용자 SID)인지 확인하고, 아니거나
  확인할 수 없으면 연결을 끊습니다(`yangpa-rpc`의 `RpcClient::connect`가 하며, 다른 클라이언트도 같은 확인을 해야 함).
  Unix도 같습니다: 기본 경로는 0700 폴더라 남이 차지할 수 없지만 `--endpoint /tmp/…`처럼 공용 폴더를 쓰면 그럴 수 있어,
  연결 직후 소켓 서버의 유효 사용자(`SO_PEERCRED`·`getpeereid`)가 내 `geteuid()`와 다르거나 확인할 수 없으면 끊습니다.
- 데몬은 pipe를 데몬과 같은 사용자와 SYSTEM만 열 수 있게 만듭니다(DACL `D:P(A;;GA;;;<사용자 SID>)(A;;GA;;;SY)`). 보안 설명자 없이
  만들면 Windows 기본값이 Everyone·Anonymous에게 읽기를 주어, 다른 계정이 인증하지 못하는 읽기 연결만으로 연결 자리를 모두
  차지할 수 있습니다. 원격 클라이언트는 받지 않습니다(`PIPE_REJECT_REMOTE_CLIENTS`).
- 프레이밍: `u32 BE 길이 | u8 종류 | 본문`. 종류 `1` = JSON-RPC 메시지(UTF-8), 종류 `2` = 바이너리
  (`u64 BE 요청 ID | 바이트`). 프레임 최대 16MiB.
- 연결 후 첫 호출은 `auth.hello`. 토큰 없이 가능한 호출은 `auth.hello`, `system.version`뿐입니다.
- 동시 연결 64개, 연결 하나의 동시 요청 32개(넘은 연결은 바로 닫고, 넘은 요청은 자리가 날 때까지 읽지 않음). 요청 자리는 그
  응답을 소켓에 쓴 뒤에 돌려주므로, 응답을 읽지 않는 클라이언트의 연결에는 응답이 32개까지만 쌓입니다(그 뒤 쓰기 시간
  초과 30초에 닫힘). 연결을 끊어도 처리 중인 요청은 끝까지 돌고, 끝날 때까지 그 연결 자리를 쥡니다.
- 소유자 토큰은 데몬이 처음 실행될 때 `<데이터 경로>/owner.token`(0600)에 만들고, GUI·CLI가 읽어 씁니다.

## 공통 규칙

- 식별자는 base58 문자열(`UserId`, `SpaceId`, `ChannelId`, `EventId`, `BlobId`, `RoleId`), 이모지 ID는 10진수 문자열.
- 시각은 유닉스 밀리초 정수.
- 오류 코드: `-32600~-32603`(JSON-RPC 표준), `1001` 인증 필요, `1002` 범위 밖(토큰), `1003` 권한 없음(공간 권한),
  `1004` 없음, `1005` 잘못된 입력, `1006` 사람 확인 대기(`data.confirmation_id`), `1007` 휘발성 채널 API 읽기 거부,
  `1008` 아직 신원 없음, `1009` 네트워크 오류, `1010` 충돌·대기(커밋 확정 노드 오프라인, 체크포인트로 들어온 크루를 다른
  멤버와 확인하는 중이라 아직 쓰지 못함 등).
- 데몬에 아직 구현되지 않은 메서드는 `-32603`과 `data.not_implemented`(메서드 이름)로 답합니다(개발 중 표식).

## 토큰 범위

`scopes`: `read`, `write`, `admin`(역할·추방·밴·신고 처리·초대·채널 관리), `settings`(시더·피어·망 설정·토큰), `owner`(전부).
`spaces`: 비우면 전체, 아니면 허용 공간 ID 목록. `automation: true` 토큰은 AI용으로, 파괴적 작업이 사람 확인 대기열로 갑니다.
데몬이 열 로컬 파일 경로(맨 위나 `attachments` 항목의 `path`·`*_path`, 빈 값 빼고)는 `owner` 범위 토큰만 넘깁니다(아니면
`1002`). 데몬은 받은 경로의 아무 파일이나 읽고 써서, 다른 토큰이 `owner.token`을 첨부로 올려 읽으면 소유자가 되기 때문입니다.
그런 토큰은 첨부를 `blob.paste`로 올리고, 받은 첨부는 `blob.read`로 읽어 직접 씁니다.
인증한 모든 호출은 감사 로그에 남습니다(메서드, 토큰 이름, 결과, 시각; 인자·메시지 내용은 남기지 않음). 서버가 직접 답하는
`auth.hello`·`events.subscribe`/`unsubscribe`와 범위 검사 거절도 남고, 인증하지 않은 연결은 `auth.hello` 실패만 남습니다.

## 메서드

표기: `메서드(인자) → 결과`. `?`는 선택.

### 시스템·인증
- `auth.hello({token}) → {scopes, automation}`
- `system.version() → {version, protocol}`
- `system.status() → {identity?: UserId, device?: DeviceId, net: NetStatus, spaces: number, tor: TorStatus}`
- `system.shutdown() → {}` (owner)
- `diag.metrics() → {delivery_ms_p50, delivery_ms_p95, memory_bytes, cpu_percent, queues: {...}, events_per_sec}`
  — 지연은 피어가 보낸 메시지의 작성→적용 시간(최근 1024개), `events_per_sec`는 지난 10초 평균, `cpu_percent`는 지난 호출 뒤
  평균(첫 호출은 0). `queues`: `ingest_held`, `ingest_waiting`, `volatile_channels`, `volatile_events`, `volatile_event_bytes`, `volatile_keys`,
  `volatile_blobs`, `volatile_blob_bytes`(RAM에 있는 휘발성 첨부 암호문), `volatile_blob_reserved`(받는 중인 첨부는 크기만큼, 받기를 쉬는 첨부는 가진 만큼 잡아 둔 예산)
- `diag.leakcheck({marker}) → {clean: bool, findings: string[]}` — 휘발성 표식 문자열(6~1024바이트, 아니면 `1005`)이 디스크·로그에
  있는지 검사(owner). 원문·소문자 hex·UTF-16LE로 찾고(SQLite `-shm`의 잠금 바이트 16바이트만 건너뜀), 표식은 어디에도 기록하지 않습니다. `findings`는 위치만:
  `db:<표>.<열>`, `db:<열>(복호화)`(저장 키로 봉인된 열을 풀어서), `file:<데이터 경로 기준 상대 경로>`, `log:<경로>`(`--log-file`이
  데이터 경로 밖일 때), `blob:<블롭 ID 앞부분>`(키를 아는 디스크 블롭을 풀어서. 풀리지 않으면 뒤에 `(복호화 실패: 검사 불가)`),
  `blob-store:<블롭 ID 앞부분>(휘발성 첨부)`(지금 RAM에 있는 휘발성 첨부가 디스크 블롭 저장소에도 있음)

### 신원·기기
- `identity.create({name}) → {user: UserId, device: DeviceId}` — 새 계정을 만듭니다. 계정은 내 기기에만 있어 기기를 모두
  잃으면 사라집니다. 쓸 기기를 더하려면 아래 `device.link.*`로 잇습니다
- `identity.get() → {user, device, profile: Profile, devices: DeviceEntry[], first_syncing}` — `first_syncing`: 내 기기로 연결된 뒤
  첫 동기화 중. 앱은 이 동안 로딩 화면을 보여 주고, 끝나면 `device.synced`가 옵니다(데몬을 다시 켜면 꺼짐)
- `device.link.create() → {url, qr: {size, modules}, expires_at}` (owner, 사람만) — 새 기기를 들일 일회용 링크를 띄웁니다.
  토큰은 RAM에만 있고 10분 뒤 끝나며, 앞서 띄운 링크도 끝납니다(protocol.md "새 기기 연결"). 링크가 사는 동안 직접 스웜을 띄워 두고 그 주소(IP)를
  링크에 담습니다. `url`은 `yangpa://device/<bs58>`이고
  `qr.modules`는 위 줄부터 `size`×`size` 칸마다 `1`(검정)·`0`(흰색)입니다. 폐기된 기기면 `1003`,
  새 기기를 승인하는 중이면 `1010`
- `device.link.pending() → {name, code, contested} | null` (owner, 사람만) — 띄운 링크로 들어와 승인을 기다리는 기기. `code`는 양쪽
  화면에 같이 뜨는 6자리 번호, `contested`는 다른 기기도 이 링크를 썼다는 뜻입니다(링크가 샜을 수 있음)
- `device.link.decide({approve}) → {}` (owner, 사람만) — 그 요청을 승인하거나 거절합니다. 어느 쪽이든 링크는 끝납니다. 승인하면
  새 기기를 내 목록에 넣어 공동 서명해 알리고, 시드·목록·프로필을 그 기기의 상자 키로 봉인해 둡니다(10분 동안 그 노드가 받아 갈
  수 있음). 기다리는 요청이 없으면 `1004`
- `device.link.cancel() → {}` (owner, 사람만) — 띄운 링크를 끝냅니다. 승인을 처리하는 중이면 그대로 둡니다
- `device.link.join({url}) → {code}` (owner, 사람만) — 신원이 없는 이 기기가 링크로 연결을 청하고 이 기기 화면에 띄울 번호를
  돌려줍니다. 결과는 `device.link_result`로 옵니다. 이미 신원이 있으면 `1010`, 링크가 잘못됐으면 `1005`, 쓰던 기기에 닿지 못하면
  `1009`, 다른 기기가 먼저 썼거나 링크가 끝났으면 `1003`
- `device.link.abort() → {}` (owner, 사람만) — 이 기기의 연결 요청을 그만둡니다(`device.link_result`는 오지 않음)
- `device.list() → DeviceView[]` — 폐기한 기기도 `revoked_at`과 함께 남습니다(목록은 늘기만 함, 폐기하면 이름은 빈 문자열). `DeviceView`는
  `DeviceEntry`에 `mobile`(휴대폰인지, 내 기기끼리 알려 준 값, 모르면 `false`)을 더한 것입니다. `DeviceEntry` =
  `{device, box_key, sign_keys: {ed_pk, pq_pk}, name, added_at, revoked_at?}`. `box_key`는 봉인 상자 공개키(X-Wing 1216바이트),
  `sign_keys`는 그 기기의 서명 공개키(Ed25519 32바이트, FN-DSA-512 897바이트)이고 모두 base64 문자열입니다. `device`는
  `sign_keys`의 키 ID입니다(protocol.md "기기 목록"). `identity.get`의 `devices`는 `DeviceEntry[]`입니다.
- `device.rename({device, name}) → {}` (owner, 사람만) — 내 기기 이름을 바꿔 새 서명 목록으로 알립니다(앞뒤 공백은 지움). 이름이 비었거나
  100자를 넘으면 `1005`, 내 목록에 없는 기기는 `1004`, 폐기한 기기는 `1010`, 이 기기가 폐기됐으면 `1003`
- `device.revoke({device}) → {}` (owner, 사람만) — 폐기 표시를 단 새 서명 목록을 모든 공간에 넣어 확정 기기가 키
  그룹에서 그 기기를 빼고(묶음 창을 기다리지 않는 즉시 회전) 상태 키를 바꾸며, 친구·멤버·내 기기는 그 기기의 인증을 끊고 다시 받지 않습니다. 폐기된 기기는 다음에
  연결해 폐기를 확인하면 로컬 데이터를 모두 지우고 다시 뜹니다(`device.wiped`, `identity.delete`와 같음). 폐기된 기기가 확정 기기였으면 다른 기기가 확정 기기를 넘겨받아
  키 그룹 표를 다시 세웁니다(공개 끝으로 이어받고 전체 재키). 지금 쓰는 기기는 `1005`, 내 목록에 없는 기기는 `1004`, 이미 폐기했으면 `1010`
- `identity.delete() → {}` (owner, 사람만) — 계정 삭제: 이 기기의 신원·기기 키·공간·메시지·블롭·설정을 지우고 `device.wiped`를
  보냅니다(네트워크 설정만 남음). 약 3초 뒤 데몬이 다시 떠 새 PeerId와 새 onion 주소를 씁니다. 서버가 없어 내 다른 기기의 계정과 남이 이미 받은 메시지는 지우지 못합니다. 내 다른 기기가 있으면
  그 기기에서 링크를 띄워 이 기기를 다시 들일 수 있고, 마지막 기기였으면 계정은 사라집니다.
  Play의 앱 안 계정 삭제 요구를 맞춥니다

### 후원자 확인
앱의 라이브러리 탭·콘텐츠 뷰어를 GitHub Sponsors·Patreon **정기** 후원자와 GitHub 3달러 이상 1회 후원자(결제 뒤 30일)에게만 엽니다(decisions.md "후원자 확인"). 확인은 이
데몬이 직접(GitHub는 Tor로 도는 기기면 Arti, Patreon은 Tor 출구를 막아 직접 연결) 하고, 하루에 한 번 다시 합니다. 데몬 API·CLI·MCP는 막지 않습니다.
- `supporter.status() → {active, grace_until?, providers: ("github"|"patreon")[], links: SupporterLink[], shared: SupporterShared[]}`
  (owner: 연결한 계정 이름이 실려 읽기·AI 토큰에는 주지 않음) — `active`면 쓸 수 있음(이 기기나 내 다른 기기의 후원 확인,
  기존 사용자 유예 `grace_until`, 또는 `providers`가 빈 빌드). `SupporterShared`: `{device, provider, login, active, valid_until}`
  — 내 다른 기기(`device`)가 확인해 보내 준 결과(그 기기만 로그인하면 됨). `SupporterLink`:
  `{provider, login, active, checked_at, valid_until, needs_login}`. `valid_until`까지 확인 결과를 믿고(GitHub 3일, Patreon 다음
  결제일 하루 뒤), `needs_login`이면 토큰이 끊겨 다시 로그인해야 함
- `supporter.github_begin() → {user_code, verification_uri, expires_in_secs}` (owner, 사람만) — GitHub 기기 로그인. 사용자가
  `verification_uri`에서 `user_code`를 넣고 승인하면 연결하고 `supporter.updated`. 이 빌드가 GitHub를 쓰지 않으면 `1005`,
  GitHub에 닿지 못하면 `1009`. 다시 부르면 이전 로그인을 멈춤
- `supporter.patreon_begin() → {url}` (owner, 사람만) — Patreon PKCE 로그인 주소(브라우저로 엶). 승인하면 브라우저가
  `http://127.0.0.1:47900/callback`으로 돌아오고, 연결하면 `supporter.updated`. 10분 안에 승인하지 않으면 실패. 그 포트를 다른
  프로그램이 쓰면 `1010`, 이 빌드가 Patreon을 쓰지 않으면 `1005`
- `supporter.refresh() → supporter.status` (owner) — 연결한 계정을 지금 다시 확인(닿지 못하면 마지막 결과 그대로)
- `supporter.unlink({provider}) → {}` (owner, 사람만) — 그 연결과 토큰을 이 기기에서 지움

### 앱 업데이트
공개 저장소(`dhtpdud/YANGPA`)의 GitHub 릴리스를 봅니다. Tor로 도는 기기는 Arti로만(받기 한 번마다 새 회로) 보내고, 오프라인
모드에서는 `1009`(decisions.md "앱 업데이트"). 설치는 앱 셸(`update_install`)이 받은 파일의 서명·SHA-256을 다시 확인한 뒤 합니다.
- `update.check() → {current, latest, newer}` (owner) — `releases/latest`가 넘겨주는 태그(`v<x.y.z>`, 정식 판만)를 읽음.
  `newer`면 `latest`가 이 데몬 판(`current`)보다 새 판. GitHub에 닿지 못하면 `1009`
- `update.download({version, asset}) → {}` (owner, 사람만) — 그 판의 릴리스 파일(`YANGPA_<version>_…`)을 데이터 폴더
  `updates/`에 받기 시작. 먼저 `SHA256SUMS.txt`와 `SHA256SUMS.txt.sig`(배포자 Ed25519 서명)를 받아 확인하고 그 목록에 있는
  파일만 받으며, SHA-256이 맞아야 제 이름으로 둠. 연결이 끊기거나 60초 동안 멈추면 새 회로로 받은 데부터 이어 받음
  (`Range`, 받은 양이 늘지 않은 시도가 6번 이어지면 `network`). 진행·끝은 `update.progress`. 이름이 그 꼴이 아니면 `-32602`, 이미 받는
  중이면 그대로 둠. 데몬이 뜰 때 `updates/`를 지움

### 후원자 표시
월 정기 후원자의 프로필 카드 배너(14달러 이상)와 프로필 사진 후광(50달러 이상). GitHub 1회 후원도 같은 금액 이상이면 결제 뒤 30일 동안. 규칙은 protocol.md "후원자 표시", decisions.md
"후원자 카드 배너와 후광".
- `flair.list() → FlairView[]` (read) — 지금 무언가 보이는 사용자(나 자신도). `FlairView`: `{user, banner, halo_allowed, halo?:
  [색, 색], until}`. `halo`는 후광을 쓸 수 있고 켰을 때만(고르지 않았으면 기본 색). `until`(ms)이 지나면 둘 다 끈 것으로 봅니다
- `flair.set({halo: [색, 색] | null}) → FlairView` (settings, 사람 토큰만) — 내 후광 색(0xRRGGBB, `null`이면 끔). 지금 후광을
  쓸 수 없으면 `1003` "후광은 월 50달러 이상 정기 후원자와 GitHub에서 50달러 이상 1회 후원한 사람(30일 동안)만 쓸 수 있습니다", 색이 범위를 넘으면 `1005`. 내 공간마다
  `MemberFlair`로 퍼뜨리고 `flair` 알림

### 표시 상태
이 기기가 다른 사람 화면에 어떻게 보일지(기기마다). 규칙은 protocol.md "표시 상태", decisions.md "표시 상태와 마지막 동기화".
- `presence.get() → PresenceView` (read) — `{mode: "online" | "away" | "invisible" | "offline", until?, idle, hide_last_seen, offline}`.
  `online`은 자동(앱이 알린 `idle`이면 자리 비움으로 보임)이고 기본값. `until`(ms)이 지나면 `online`으로 돌아갑니다. `offline`은
  이 데몬이 망 없이 떠 있다는 뜻(오프라인 모드로 시작함)
- `presence.set({mode?, duration_secs?, idle?, hide_last_seen?}) → PresenceView` (settings, 사람 토큰만) — 준 것만 바꿉니다.
  `duration_secs`는 온라인이 아닌 `mode`와 함께(1초~365일, 아니면 `1005` "기간은 1초에서 365일 사이여야 합니다", 없으면 바꿀
  때까지). `idle`은 앱이 기기의 마지막 입력 시각을 보고 알림(데스크톱 앱은 10분). `hide_last_seen: true`면 다른 사람 화면에
  나와의 마지막 동기화 시각이 보이지 않습니다. 바뀌면 `presence.self`와 내 사용자의 `presence`가 가고 인증된 친구·멤버 기기에
  알립니다
  - `mode: "invisible"`(숨기): 연결·동기화·대화는 그대로이고 다른 사람 화면에는 오프라인
  - `mode: "offline"`: 응답 뒤 데몬이 멈추고 같은 인자로 다시 뜨면서 망(Tor·P2P 스웜 포함)을 띄우지 않습니다. 오프라인에서
    다른 모드로 바꾸거나 기간이 끝나도 같은 방식으로 다시 떠 망을 켭니다(앱은 연결이 끊겼다 다시 붙음). 오프라인 동안
    `supporter.github_begin`·`supporter.patreon_begin`·`supporter.refresh`는 `1009` "오프라인 모드에서는 할 수
    없습니다"이고 후원 확인·P2P 직접 연결 스웜도 돌지 않습니다. 쓴 글은 이 기기에 남고 다시 온라인이 되면 동기화됩니다

### 뱃지
도전과제 뱃지. 규칙은 protocol.md "뱃지 카드", decisions.md "도전과제 뱃지". `Badge`는 `"founder"`, `"support3"`·`"support14"`·
`"support50"`, `"friends5"`…`"friends100"`, `"crew10"`…`"crew10000"`, `"seed1gb"`…`"seed1pb"`, `"voice1h"`…`"voice10000h"`,
`"screen1h"`…`"screen10000h"`(번호 순).
- `badge.get({user}) → BadgeView` (read) — `{earned: [Badge], shown: [{badge, after}], can_grant_founder}`. 모르는 사람이면 빈
  목록. 원년멤버는 개발자 증서가 맞을 때만 들어 있고, 나머지는 그 사람 데몬이 계산해 알린 값입니다. 내 사용자면 기준(친구·소유한
  크루 멤버 수·시딩·통화·화면 공유 시간·후원액)을 지금 다시 봅니다. `can_grant_founder`는 내가 개발자 계정이고 그 사람에게 아직 주지 않았을 때
- `badge.set_shown({shown: [{badge, after}]}) → BadgeView` (settings, 사람 토큰만) — 이름 앞(`after: false`)이나 뒤에 보일
  뱃지를 통째로 바꿉니다. 두 개까지(`1005` "뱃지는 두 개까지 보일 수 있습니다"), 같은 뱃지 두 번 금지, 모은 것만(`1005` "모으지
  않은 뱃지는 보일 수 없습니다"). 빈 목록이면 보이지 않음
- `badge.grant_founder({user}) → {}` (owner, 사람 토큰만) — 개발자 계정만(`1003` "개발자 계정만 원년멤버를 줄 수 있습니다").
  증서를 서명해 남기고 그 사람의 인증된 기기에 보냅니다(지금 없으면 다음 인증 때). 나 자신에게도 줄 수 있고 되돌리기는 없습니다

### 프로필·메모
- `profile.get({user?}) → Profile`
- `profile.set({name?, bio?, avatar_path?, banner_path?, theme_color?, status?}) → Profile` — 지금 인증된 친구·멤버·내 기기에
  바로 보내고, 셋 이상인 공간(크루 등)에는 상태 이벤트(`MemberProfile`)로 실어 직접 연결되지 않은 멤버도 동기화로 받습니다.
  받는 쪽은 `profile.updated`를 알리고, 친구의 새 이미지는 바로 받기 시작합니다. 이 기기도 `profile.updated`(내 사용자)를 알려
  내 화면의 멤버 목록·메시지 이름이 바로 바뀝니다(상태 이벤트를 내지 않는 1대1·작은 공간에는 다른 알림이 없음).
  연결이 없던 친구와는 다음 인증 때 서로 주고받습니다. 이미지 경로는 절대 경로, 8MiB 이하 png·jpeg·gif·webp(내용으로 판별, 첨부 설정과 상관없이 메타데이터를 지워 올림, 깨져 지우지 못하면 `1005`),
  `""`이면 지움. 이미지 복호화 키는 서명된 프로필에 실려 친구·같은 공간 멤버에게 가지만 로컬 API의 `Profile`(JSON)에는 나오지
  않습니다(`avatar_key`·`banner_key`는 늘 `null`). 이름을 바꿀 때 그림 문자(이모지)가 있으면 `1005` "이름에 이모지를 넣을 수
  없습니다"(`identity.create`의 `name`도 같음, 뱃지와 헷갈리지 않게)
- `profile.set_crew({space, nickname?}) → {}` — 크루에서만(아니면 `1005`). 별명은 그 크루 상태의 내 멤버 항목에 들어가 그 크루
  멤버에게만 보입니다(역할을 싣지 않는 `MemberProfile` 이벤트라 동시에 바뀐 역할과 부딪히지 않음). `nickname`이 없거나 빈
  문자열이면 지움, 1~64자·제어 문자 금지, 그림 문자는 `1005` "별명에 이모지를 넣을 수 없습니다"
- `note.get({user}) → {text}` / `note.set({user, text}) → {}` — 다른 사용자에 대한 개인 메모(없으면 빈 글, 빈 글로 쓰면 지움,
  64KiB 이하, 나 자신에게는 `1005`). 공간에 올리지 않고 내 기기끼리만 봉인해 맞춥니다(protocol.md "개인 메모")

### 친구
연락처 링크로 만난 사람은 상대의 수락 없이 바로 **임시 연락처**가 되어 1:1 공간이 생기고, 둘 다 `friend.propose`해야
**친구**가 됩니다. 링크에 만료가 있으면 친구가 되지 않은 임시 연락처는 그때 **만료된 연락처**가 되어 그 1:1 공간이 비활성화됩니다
(보내기·동기화 안 함, 지난 대화는 읽기만. 그 공간에 쓰는 호출은 `1003`). 흐름은 protocol.md "연락처 링크와 친구".
- `friend.link.create({expires_in_secs?, single_use?}) → {url, token_id, expires_at?}` — 연락처 링크. 만료가 곧 이 링크로 생긴
  임시 연락처의 만료입니다(없으면 만료 없음)
- `friend.link.list() → {token_id, created_at, expires_at?, single_use, uses}[]`
- `friend.link.revoke({token_id}) → {}` — 그 링크로 생긴 임시 연락처도 바로 만료합니다(상대 기기에 알림). 없는 링크면 `1004`
- `friend.add({url}) → {status: "pending" | "connected", user}` — 링크를 씁니다. 링크 주인이 받아 1:1 공간을 만들어 보내면
  임시 연락처가 되고 `friend.accepted`가 옵니다. 닿지 않아도(꺼져 있음, Tor 링크를 막 만듦) `pending`으로 끝나고 데몬이
  백그라운드로 다시 보냅니다(연결될 때·백오프마다, 7일 또는 링크 만료까지). 이미 친구이거나 이 링크로 이미 임시 연락처면
  보내지 않고 `connected`. 만료된 연락처가 새 링크를 쓰면 같은 1:1 공간으로 다시 임시 연락처가 됩니다. 거절(만료·폐기된 링크,
  상대의 임시 연락처가 256명으로 가득 참 등)·내 링크는 오류
- `friend.propose({user}) → {status: "requested" | "friends"}` — 임시 연락처에게 친구 추가를 보냅니다. 상대도 보냈으면(또는 상대가
  이미 나를 친구로 보면) 바로 `friends`, 아니면 상대의 수락을 기다리는 `requested`(닿지 않으면 다음 연결 때 보냄). 이미 친구면
  `friends`, 만료된 연락처면 `1005`, 연락처가 아니면 `1004`. 받은 쪽에는 `friend.request`로 알려 `friend.requests`에 보입니다
- `friend.open({user}) → {space}` — 그 연락처와 대화할 1:1 공간. 보통은 `FriendView.space` 그대로입니다. 1:1을 크루로
  바꿔(`crew.convert_*`) 기록이 크루를 가리키면(그 크루에서 나갔어도) 둘만의 1:1 공간을 새로 만들어 돌려주고 `friend.updated`를 알립니다. 상대에게는
  데몬이 보내고(닿지 않으면 다음 연결 때), 상대 쪽 기록도 그 공간으로 옮겨 갑니다. 둘이 따로 만들었으면 양쪽이 같은 하나로
  맞춥니다. 만료된 연락처는 지난 공간 그대로(읽기만), 연락처가 아니면 `1004`
- `friend.requests() → {request_id, user, profile, received_at}[]` — 받은 친구 추가 요청. 내 기기 하나에만 닿아도 내 기기끼리 맞춰 모두에 보입니다
- `friend.accept({request_id}) → {space}` / `friend.reject({request_id}) → {}` — 친구 추가 요청을 수락하면 둘 다 친구가 됩니다
  (`friend.propose`와 같음). 거절은 내 다른 기기로도 가고, 같은 사람이 다시 보내도 보이지 않습니다(상대에게는 알리지 않음)
- `friend.list() → FriendView[]` — 친구·임시 연락처·만료된 연락처. `FriendView = {user, profile, space, online, away?, last_seen?,
  safety_number_verified, kind: "friend" | "temporary" | "expired", expires_at?, proposed}` — `expires_at`은 임시 연락처의 만료
  시각(만료 없음이면 없음), `proposed`는 내가 친구 추가를 보내고 기다리는 중. 옛 응답에 `kind`가 없으면 `friend`.
  `online`·`away`·`last_seen`은 아래 `MemberView`와 같음
- `friend.remove({user}) → {}` (파괴적: 자동화 토큰은 사람 확인 `1006`) — 친구·임시 연락처 관계를 끊고 양쪽의 1:1 공간과 그 대화·파일을 지웁니다. 상대와 내 다른 기기도
  같게 지웁니다(지금 닿지 않으면 다음 연결 때 전함). 크루에서도 만나지 않는 사람이면 프로필·기기 목록·개인 메모도 지웁니다. 받은 쪽에는 `friend.updated`와 `space.updated`가 갑니다.
  지운 공간의 `message.*`·`space.get`은 `1003`이고, 다시 만나면 새 1:1 공간을 씁니다. 1:1을 크루로 바꿨으면 크루는 그대로 두고 연락처만 지웁니다.
  만료된 연락처는 상대에게 전하지 않습니다. 그 사람의 신뢰 스위치도 끕니다. 연락처가 아니면 `1004`
- `friend.safety_number({user}) → {digits: string, verified: bool}` / `friend.verify({user, verified}) → {}`

### 신뢰 스위치
이 기기의 IP를 보여도 되는 사람·크루를 고릅니다(기기마다, 기본은 꺼짐). 서로 신뢰하면 P2P, 한쪽만 신뢰하면 단방향(신뢰한 쪽의
IP만 보임), 아무도 신뢰하지 않으면 Tor입니다. 크루 신뢰는 그 크루 멤버에게만 적용됩니다. 규칙은 protocol.md "신뢰 스위치와
직접 연결". 앱은 켤 때마다 IP 노출 경고를 띄웁니다("다시 보지 않기"는 앱 설정). 어떤 응답에도 IP는 실리지 않습니다.
- `trust.get({user}) → TrustView` (read) — `{user, trusted, via_crews, connection}`. `trusted`는 사람 스위치, `via_crews`는 그
  사람이 멤버인 크루 중 내가 신뢰한 크루(나 자신이면 빈 목록), `connection`은 그 사람의 기기와 지금 이어진 가장 빠른 방식:
  `"p2p"`(서로 신뢰 또는 함께 참여한 P2P 전용 채널) · `"one_way_me_exposed"`(나만 신뢰, 내 IP만 보임) ·
  `"one_way_peer_exposed"`(상대만 신뢰, 상대 IP만 보임) · `"tor"` · `"offline"`(이어진 기기 없음). 순서는 `p2p` > 단방향 > `tor`
- `trust.set_user({user, enabled}) → {}` (write, 사람 토큰만) — 사람 스위치. 켤 때는 함께 멤버인 활성 공간(친구·연락처의 1:1
  공간이나 크루)이 있어야 하고(`1004`), 나 자신은 `1005`, 4096명까지(`1010`). 이미 그 값이면 그대로 성공. 친구·연락처를
  지우면(`friend.remove`) 꺼집니다
- `trust.set_space({space, enabled}) → {}` (write, 사람 토큰만) — 크루 스위치. 내가 멤버인 크루만(1:1 공간은 `1005`, 사람
  스위치로), 4096개까지(`1010`). 값이 바뀌면 `space.updated`. 크루를 나가면 꺼집니다. 지금 값은 `SpaceView.trusted`

### 공간·크루
- `space.list() → SpaceSummary[]` — `{space, kind, name, icon?, unread, mention, dormant, member_count}`
- `space.get({space}) → SpaceView` — 해소된 상태(멤버·역할·채널 트리·이모지·설정), 내 권한, 키 갱신 대기 여부, 이 기기가 그
  크루를 신뢰하는지(`trusted`, `trust.set_space`). 역할 표시 설정(`role.update`의 `visible_to`·`see_admins`)에 따라 내게 보이지
  않는 역할은 `roles`와 멤버의 `roles`에서 빠집니다(`member.list`·`role.list`도 같음). `admins_hidden: true`면 내가 관리자를 볼 수
  없다는 뜻이고, 관리자 권한 역할도 빠져 있습니다(앱은 소유자 표시도 숨김). `role_display`(`{역할: {visible_to, see_admins}}`,
  기본값이 아닌 역할만)는 역할을 관리할 수 있는 사람(소유자·관리자·`manage_roles`)에게만 채웁니다. 화면에서만 거르는 것이라
  누가 어떤 역할인지는 멤버 기기의 크루 상태에 그대로 있습니다
  - 크루 STUN 서버: `crew_servers`(`{stun: string[]}`)는 `manage_space`가 있는 사람에게만(목록이 비어도 `{stun: []}`)
    채웁니다. `crew_stun`은 크루 STUN 서버가 있고 내 역할이 쓸 수 있는지(`use_crew_stun`), `use_crew_servers`는 이 기기가
    쓰기로 했는지(`crew.use_servers`)입니다
  - `disbanded: true`면 소유자가 해산한 크루입니다(`crew.disband`). `my_permissions`가 비고 채널도 보이지 않으며, `crew.leave`로
    이 기기에서 지웁니다(거짓이면 생략)
  - 키 갱신: `pending_commit`은 이 기기가 아직 크루 키 그룹 키를 받지 못했거나(가입 갱신 대기) 확정 기기가 반영할 상태
    변화(가입·제거)가 남은 것입니다. `key_rotation_pending`은 이 기기가 최근 메시지를 **제거가 아직 반영되지 않은 키**로
    보냈다는 뜻입니다: 추방·밴·기기 폐기·채널 권한 회수 뒤 확정 기기의 회전(`KeyUpdate`)을 `rotation_wait`(30초)까지
    기다렸다가, 그때도 오지 않으면 옛 키로 보내고 이 값을 켭니다(빠진 기기가 그 메시지를 읽을 수 있음). 회전이 오면 꺼지고
    `space.updated`가 갑니다. 크루 설정 `strict_rotation`이 켜져 있으면 30초에서 멈추지 않고 회전이 올 때까지 끝없이
    기다리며 옛 키로 보내지 않습니다(`message.send`가 그동안 돌아오지 않음, 아래 `crew.update`)
- `crew.create({name, description?}) → {space}` — 멤버 1명짜리 크루와 첫 텍스트 채널 `일반`. 이름(크루·채널·역할·게시글
  제목 공통)은 앞뒤 공백을 뺀 1~100자, 제어 문자 불가(`1005`)
- `crew.update({space, name?, description?, icon_path?, strict_rotation?, emoji_limits?}) → {}` — `manage_space`. `strict_rotation: true`면
  제거를 반영한 키 회전이 올 때까지 그 채널에 보내는 `message.send`가 끝없이 기다립니다(기본 꺼짐: 30초 뒤 옛 키로 보내고
  `key_rotation_pending`). 설정은 `settings.strict_rotation`. `icon_path`(절대 경로, 메타데이터는 늘 지움, png·jpeg·gif·webp
  8MiB 이하)는 이 크루 공간의 블롭으로 들여오고 키는 공간 설정(상태 레인)에 실어 멤버만 엽니다. 멤버 노드는 아이콘을 자동으로
  받고 `blob.read`(`key_from`은 아무 값)로 엽니다. `settings.icon_key`는 JSON에서 늘 `null`. `icon_path: ""`는 아이콘 지우기.
  `emoji_limits: {max_count, max_bytes}`는 크루 이모지 한도를 통째로 바꿉니다(`EmojiLimitsSet`): `max_count`는 1~500(`null`이면
  500까지), `max_bytes`는 이모지 하나의 최대 바이트(1 이상, `null`이면 무제한), 틀리면 아무것도 바꾸지 않고 `1005`.
  `SpaceView.emoji_limits`로 읽습니다(빈 값 `{max_count: null, max_bytes: null}`)
- `crew.set_servers({space, stun: string[]}) → {}` (admin, 사람 토큰만) — `manage_space`, 크루만. 크루 STUN 목록 전체를
  바꿉니다(상태 이벤트 `CrewServersSet`). 줄 꼴과 맞추는 규칙은 `net.set_stun_servers`(`host:port`)와 같고(TURN 줄은 `1005`), 8줄,
  줄마다 512바이트까지(`1005`). 지금 목록과 같으면 아무것도 내지 않고 성공
- `crew.use_servers({space, enabled}) → {}` (settings, 사람 토큰만) — 이 기기가 그 크루의 STUN 서버를 쓸지(이 기기에만, 설정
  키는 예전 이름 그대로 `turn.crews`). 크루 멤버만, 1:1 공간은 `1005`. 켜도 역할 권한 `use_crew_stun`이 있을 때만 씁니다. 직접
  스웜의 공인 주소 묻기에는 내 STUN 목록 뒤에 쓰기로 한 모든 크루의 STUN을 붙이고, 통화에는 그 크루의 STUN을 내 것 뒤에
  붙입니다. 크루 서버가 이 기기의 공인 IP를 보게 됩니다
- `crew.convert_request({space, name}) → {}` / `crew.convert_accept({space}) → {}` (AI면 사람 확인) — 1대1 → 크루(양측 동의).
  이미 상대가 요청했으면 요청 대신 수락하라고 `1010`, 요청이 없거나 자기 요청을 수락하면 `1010`. 전환 뒤 요청한 쪽이 소유자,
  수락한 쪽이 관리자 역할. 전환하면 기존 채널은 모두 과거 기록 비공개가 되고, 그 채널을 다시 공개하려면 원래 두 사람이 모두
  `channel.update`로 공개해야 함(한 사람만 하면 호출은 성공하지만 상대가 할 때까지 비공개로 남음)
- `crew.leave({space}) → {}` (AI면 사람 확인) — 소유자는 먼저 넘겨야 함(`1010`). 탈퇴 이벤트를 다른 멤버 노드 하나 이상이 받아야 끝나며
  (못 보내면 `1009`, 다시 부르면 저장된 탈퇴를 다시 보냄) 그 뒤 이 기기의 공간 데이터(이벤트·키·보기·설정·키 그룹 상태)를
  지웁니다. 추방된 공간·가입 대기 중인 공간에서는 지우기만 합니다. 해산한 크루(`SpaceView.disbanded`)는 소유자도 나갈 수 있고
  탈퇴 이벤트 없이 지웁니다(해산한 사용자의 기기는 저장된 해산 이벤트를 다시 전한 뒤, 못 전하면 `1009`). 1:1에서 바꾼 크루면 나간 뒤 그 상대와 둘만의 1:1 공간을
  새로 만듭니다(`friend.open`과 같음, 친구 목록에서 사라지지 않게)
- `crew.disband({space}) → {}` (AI면 사람 확인) — 크루 해산. 소유자만(`1003`).
  해산 이벤트(`CrewDisband`)를 내면 모든 멤버 노드에서 크루가 끝납니다: 그 뒤 상태 이벤트는 모두 거부되고 소유자를 포함해 아무
  권한이 없으며, 멤버 기기는 기록을 남긴 채 `disbanded`로 보이다가 각자 `crew.leave`로 지웁니다. 해산 이벤트를 다른 멤버 노드 하나
  이상이 받아야 이 기기의 데이터를 지우고(못 보내면 `1009`, 크루는 해산한 채 남고 `crew.leave`가 다시 보냄), 1:1에서 바꾼 크루면
  `crew.leave`처럼 그 상대와 1:1 공간을 새로 만듭니다. 해산한 크루의 다른 API는 `1003`
- `crew.transfer_ownership({space, user}) → {}` (AI면 사람 확인) — 소유자만(`1003`), 소유자의 어느 기기에서나. 지금 확정
  기기가 관리자 기기가 아니면 새 소유자의 기기로 넘어갑니다. 옛 확정 기기는 키 그룹 표를 새 확정 기기에게 봉인해 넘기고
  (`KeyTableHandover`), 새 확정 기기는 자기가 볼 수 있게 된 비공개 채널 그룹에 스스로 들어갑니다(과거 기록 공개 채널은 그룹
  R에서 지난 뿌리를 파생). 표를 받지 못하면(옛 확정 기기가 꺼져 있음) 20초 뒤 공개 끝으로 이어받고 전체 재키합니다
- `crew.set_committer({space, device}) → {}` (admin) — 커밋 확정 기기를 정합니다(`CommitterChange`, 크루와 1:1 공간 모두).
  소유자의 기기에서만(`1003`), 대상은 이 공간의 소유자·관리자 활성 기기(활성 기기가 아니면 `1004`, 소유자·관리자가 아니면
  `1005`). 옛 확정 기기가 키 그룹 표를 넘기고, 넘겨받지 못하면 새 확정 기기가 공개 끝으로 이어받아 전체 재키합니다
- `crew.committer_candidates({space}) → CommitterCandidate[]` (read) — 확정 기기가 될 수 있는 기기(소유자·관리자의 활성 기기)와
  지금 확정 기기. 멤버면 누구나(아니면 `1003`), 단 관리자가 누구인지 볼 수 없는 멤버(`SpaceView.admins_hidden`)에게는 빈 목록. `CommitterCandidate = {user, device, name, online, current, mine}` — `name`은 기기
  목록의 이름(없으면 빈 문자열), `online`은 지금 인증된 연결이 있는지(이 기기면 늘 `true`), `mine`은 이 데몬의 기기.
  지금 확정 기기 → 소유자 기기 → 나머지 순, 같은 묶음은 이름순
- `member.list({space}) → MemberView[]` — `{user, profile, nickname?, roles, online, away?, automation, blocked, last_seen?}`
  (`blocked`: 내가 차단한 사용자, 이 기기에만 있는 정보). `online`은 화면에 보일 접속 상태: 그 사람의 기기 중 숨기가 아닌
  기기와 인증된 연결이 있음(인증 직후 상태를 받기 전 3초는 빼고, 상태를 보내지 않는 옛 빌드는 그 뒤 온라인). 나는 내 표시 상태.
  `away`는 보이는 기기가 모두 자리 비움. `last_seen`(ms)은 이 기기가 그 사람과 보이는 상태로 이어져 있던 마지막 때(마지막
  동기화)이고, 그 사람이 비공개로 했거나 기록이 없으면 없음. `space.get`의 멤버도 같음
- `member.search({space, query?, filter?, offset?, limit?}) → MemberPage` (read) — 큰 크루용 멤버 목록 쪽.
  `filter`: `all`(기본) · `online`(`online`인 멤버, 나는 내 표시 상태) · `recent`(온라인이거나 `last_seen`이 최근 7일 안인
  멤버, 마지막 동기화를 비공개로 한 사람은 빠짐). `query`는 대소문자 무시로 별명·프로필 이름에 들어 있거나 사용자 ID가 그것으로 시작하는 멤버.
  `limit` 기본 100, 최대 200. 순서는 온라인 → 최근(마지막 연결 최신 순) → 나머지, 같은 묶음은 표시 이름 순.
  `MemberPage = {members: MemberView[], total, online}` — `total`은 조건에 맞는 수, `online`은 조건과 무관한 온라인 멤버 수
- `member.kick({space, user, reason?}) → {}` (admin, AI면 사람 확인) — `kick` 권한, 대상보다 높은 순위, 소유자·자기 자신 불가.
  추방된 기기는 키 그룹에서 빠지고(묶음 창을 기다리지 않는 즉시 회전) 상태 키가 바뀌어 새 메시지·상태를 열 수 없으며, 시더는 그 기기의 요청을 거절. 추방된
  쪽에는 알림 이벤트를 보내고, 받은 기록은 그 사람이 `crew.leave`를 부를 때까지 남음(읽기 전용)
- `member.set_roles({space, user, roles}) → {}` (admin) — `manage_roles`. 더하거나 빼는 역할과 대상 멤버 모두 내 순위보다 낮아야 함
  (순위는 역할 수준으로 비교: 아래 "역할·초대"의 `role.list`)
- `member.ban({space, user, reason?, delete_recent?}) → {}` (admin, AI면 사람 확인) — 크루만, `ban` 권한. 추방하고(멤버면 기기
  제거·상태 키 회전은 추방과 같음) 밴 목록에 올립니다. 밴된 사용자는 옛 초대·새 초대로 다시 들어오지 못합니다(그 사람의
  `invite.join`이 `1003`, 모든 노드가 가입 이벤트를 거부). 멤버가 아닌 사용자도 ID로 미리 밴할 수 있습니다. 자기 자신 `1005`,
  소유자·나보다 순위가 높거나 같은 멤버 `1003`, 이미 밴됨 `1010`. `reason`은 512자까지이고 멤버 모두가 읽는 상태에 남습니다.
  `delete_recent: true`면 그 사용자가 최근 24시간 동안 쓴 일반 등급 메시지를 멤버 노드마다 지웁니다(휘발성 채널은 해당 없음)
- `member.unban({space, user}) → {}` (admin, AI면 사람 확인) — `ban` 권한. 목록에 없으면 `1004`. 밴만 풀며, 다시 들어오려면 유효한 초대가 필요
- `member.bans({space}) → BanView[]` (admin) — `ban` 권한(없으면 `1003`). 최근 것부터.
  `BanView = {user, profile: Profile | null, reason: string | null, banned_by, banned_at}` — `profile`은 받은 적 있는 프로필

### 채널
- `channel.list({space}) → ChannelView[]` — `{channel, settings: ChannelSettings, unread, joined?, participants?, api_read_allowed, p2p, p2p_joined, anonymous?, featured_post?, attach_limit?, no_edit?, no_delete?, volatile_cap?, volatile_full?, has_new?}`
  (휘발성 채널: `joined`는 입장·퇴장 방식만, `participants`는 내가 참가 중일 때 지금 참가자 사용자, `unread`는 RAM 기준.
  `p2p`는 P2P 전용 채널, `p2p_joined`는 이 기기가 `channel.p2p_join`으로 참여 중인지. 옛 응답에 없으면 `false`.
  `anonymous`는 익명 채널(거짓이면 빠짐). 익명 채널의 `participants`는 수만 맞춰 모두 `ANONYMOUS_USER`.
  `featured_post`는 포럼 채널의 대문 게시글 ID: 이 기기에 그 채널의 지우지 않은 게시글로 있을 때만.
  `attach_limit`은 크루 관리자가 정한 첨부 하나의 최대 바이트(없으면 무제한, 빠짐. 0은 첨부 금지).
  `no_edit`·`no_delete`는 수정·삭제 금지 채널(거짓이면 빠짐).
  `volatile_cap`은 상시 참여 휘발성 채널의 참가 기기 상한(다른 채널은 빠짐), `volatile_full`은 상한이 차서 이 기기가 참가하지
  못하는 중(거짓이면 빠짐, 자리가 나면 자동으로 참가).
  `has_new`는 채널 토픽(`net.channel_topics`)을 쓰는 큰 크루에서 따라가지 않는 이 채널에 이 기기가 아직 받지 않은 새 글이 있음
  (멤버가 알려 준 채널 끝 기준, 수는 모름, 거짓이면 빠짐). 새로 켜지면 `space.updated`, 채널을 열면 꺼지고 그 채널을 받아 옵니다)
- `channel.create({space, name, kind, parent?, topic?, security?, join_mode?, ttl_secs?, ttl_on_leave?, history?, link_previews?, private_roles?, position?, forum_tags?, p2p?, overwrites?, anonymous?, attach_limit?, no_edit?, no_delete?}) → {channel}`
  — `manage_channels`. `overwrites`(`{target: {type: "role" | "member", id}, allow, deny}[]`, 권한 비트 수)는 덮어쓰기 목록 전체로,
  `private_roles`와 함께 쓸 수 없습니다(`1005`). 모르는 비트, 같은 비트의 허용·거부, 겹친 대상, 없는 역할·멤버, 100개 넘게는 `1005`,
  허용·거부가 모두 0인 항목은 뺌, 내게 없는 권한 비트를 바꾸면 `1003`(소유자·관리자는 전부). 카테고리의 덮어쓰기는 안의 채널로
  이어지지 않습니다(앱이 카테고리를 따르는 채널에 같은 목록을 씀). `private_roles`가 있으면 비공개 채널(@everyone의 `view_channel` 거부 + 그 역할들에 허용, 빈 목록은 공개):
  볼 수 있는 멤버의 기기만 든 비공개 키 그룹(같은 열람 규칙의 채널끼리 하나)이 생기고 메시지 키(채널 뿌리)는 그 그룹 키에서만
  파생함. 역할 기본 권한이나 멤버별 덮어쓰기로 볼 수 없는 멤버가 하나라도 있는 채널도 같음(크루가 크고 빠지는 기기가 32대
  이하면 크루 트리 위의 덮개 그룹). 휘발성 채널의 `link_previews`
  기본은 꺼짐. `forum_tags`(`{id, name, emoji?}[]`, 20개까지)는 포럼 채널만. 포럼 채널은 휘발성 불가(게시글 변경이
  상태 레인에 남음). `ttl_secs: 0`은 수명 없음. `p2p: true`면 P2P 전용 채널: 텍스트·음성만, 셀프 공간 불가, 만든 뒤에도 내가
  관리할 수 있는 채널이어야 함(아니면 `1005`). 채널 생성과 `ChannelTransportSet`을 한 번에 냅니다. 만든 기기도 자동으로
  참여하지 않습니다(`channel.p2p_join` 필요). `anonymous: true`면 익명 채널: 크루만, 카테고리 불가(`1005`). 채널 생성 뒤
  `ChannelAnonymousSet`을 잇달아 냅니다(실패하면 만든 채널을 지움). `attach_limit`(`null`이면 무제한, 0이면 첨부 금지)은 아래
  `channel.update`와 같은 규칙이고, 빠지면 무제한이되 크루의 휘발성 채널만 10MiB입니다. `no_edit`·`no_delete`(크루만, 카테고리 불가)는
  수정·삭제 금지. 셋 다 채널 생성 뒤 따로 냅니다(`ChannelAttachLimitSet`·`ChannelEditRulesSet`)
- `channel.update({space, channel, ...같은 필드, attach_limit?, no_edit?, no_delete?, volatile_cap?}) → {}` — 종류·보안 등급은 못 바꿈(`1005`). P2P 전용 여부는 `channel.set_p2p`로 바꿈(`p2p` 필드는 받지 않음). `overwrites`는 목록을 통째로
  바꿈(빈 목록은 모두 지움). 덮어쓰기를 바꾸는 `private_roles`·`overwrites` 변경과 과거 기록을 공개로 바꾸는 `history` 변경은 AI면 사람 확인(`1006`). 1대1에서 전환된 크루의 전환 전
  채널은 원래 두 사람만 과거 기록을 공개로 바꿀 수 있고(다른 멤버는 `1003`) 둘 다 공개해야 바뀜. 한 사람이 다시 비공개로
  돌리면 그 사람의 동의는 취소됨. `anonymous`는 익명 켜기·끄기(크루만, 카테고리 불가 `1005`): 꺼도 켜져 있는 동안 쓴 글은 익명으로
  남고, 통화 채널이면 통화 중인 모든 기기가 통화에서 나옵니다(바뀌면 다른 설정이 그대로여도 `ChannelAnonymousSet`만 냄).
  `attach_limit`은 첨부 하나의 최대 바이트(`null`이면 무제한, 0이면 첨부 금지, 빠지면 그대로, 기본 무제한): 크루만, 카테고리 불가,
  0 또는 1MiB 이상(아니면 `1005`), 그 채널의 `manage_channels`. 지금과 다르면 다른 설정이 그대로여도 `ChannelAttachLimitSet`을 따로
  냄(protocol.md "채널 첨부 한도"). `no_edit`(수정 금지: 아무도 메시지를 못 고침)·`no_delete`(삭제 금지: 작성자가 자기 메시지를 못
  지움, `manage_messages`의 운영 삭제는 됨)도 같은 조건이고, 바뀌면 `ChannelEditRulesSet`을 따로 냄(protocol.md "채널 수정·삭제 금지").
  `volatile_cap`은 상시 참여 휘발성 채널의 참가 기기 상한(`null`이면 기본 200, 빠지면 그대로): 크루의 상시 참여 휘발성 채널만,
  2..=2000(아니면 `1005`), 그 채널의 `manage_channels`. 바뀌면 `ChannelVolatileCapSet`을 따로 냄(protocol.md "휘발성 채널")
- `channel.delete({space, channel}) → {}` (admin, AI면 사람 확인)
- `channel.join({space, channel}) → {}` / `channel.leave({space, channel}) → {}` — 입장·퇴장 방식 휘발성 채널(아니면 `1005`).
  `join`은 온라인 참가자에게 입장을 알리고 답을 기다린 뒤(과거 기록 공개면 남은 메시지도 받은 뒤) 돌아옵니다. `leave`는 이 기기의
  그 채널 내용·세션 키를 바로 지우고, 지운 메시지를 `message.expired`로 알립니다. 이미 들어가 있거나 나와 있으면 그대로 `{}`
- `channel.allow_api_read({space, channel, allowed}) → {}` — 휘발성 채널의 API 읽기 허용(사람만, owner 토큰). 공간·채널 ID만 설정에 저장
- `channel.mark_read({space, channel, event?}) → {}` — `event`는 그 공간·채널의 메시지여야 함(아니면 `1004`). 빼면 그 채널의 가장 최근 메시지(글타래 답글·포럼 글·댓글 포함)까지 읽음으로 하고, 메시지가 없으면 아무것도 하지 않음(앱은 채널을 열 때와 창이 돌아올 때 이렇게 부름). 휘발성 채널의 읽음 위치는 RAM에만
- `channel.typing({space, channel}) → {}` (write) — 이 채널에 글을 쓰는 중이라고 알림. 몇 초마다 불러도 되고 데몬이 채널마다 2초에
  한 번만 보냄(데스크톱 앱은 글을 치는 동안 2.5초에 한 번까지). 지금 이어진 기기 중 그 채널을 볼 수 있는 멤버 기기에만 가고(P2P 전용 채널은 함께
  참여한 P2P 피어, 휘발성 채널은 지금 참가자), 못 닿아도 오류가 아님. 받는 쪽이 `channel.typing` 알림을 냄. 모르는 공간·없는 채널
  `1004`, 멤버가 아니거나 `view_channel`·`send_messages`가 없거나 P2P 전용 채널에 참여하지 않았으면 `1003`. 저장·로그 없음
- `channel.set_p2p({space, channel, enabled}) → {}` (admin, 사람 토큰만) — 채널을 P2P 전용으로 바꾸거나(`true`) Tor로 되돌림
  (`false`). 양쪽 다 언제든 됩니다(`ChannelTransportSet`). 켤 때는 만들 때와 같은 규칙: 텍스트·음성만, 셀프 공간 불가, 그 채널의
  `manage_channels`(아니면 `1005`). 끌 때는 그 채널의 `manage_channels`만(아니면 `1003`). 없는 채널 `1004`, 멤버가 아니면
  `1003`, 이미 그 값이면 그대로 `{}`. 켜면 참여하지 않은 멤버 기기에서 그 채널이 잠기고(새 내용을 주고받지 않음), 끄면 P2P로만
  오가던 지난 기록을 Tor로 모든 멤버와 맞춥니다(이미 보인 IP는 되돌릴 수 없음). 자동화 토큰은 `1002`
- `channel.p2p_join({space, channel}) → {}` / `channel.p2p_leave({space, channel}) → {}` (write, 사람 토큰만) — 이 기기가 P2P 전용
  채널에 참여(참여자에게 IP가 보인다는 경고를 받아들임)하거나 그만둠. 참여 전에는 그 채널이 이 기기에서 잠겨 있습니다. 자동화
  토큰은 `1002`(사용자를 IP 노출에 동의시키지 못하게). `join`: 없는 채널 `1004`, 볼 수 없으면 `1003`, P2P 전용이 아니면 `1005`,
  이미 참여 중이면 그대로 `{}`. `leave`는 늘 `{}`. 참여 목록(공간·채널 ID만)은 이 기기 설정 `p2p.joined`에 두고, 채널이 P2P
  전용이 아니게 되거나 내가 멤버가 아니게 되면 지웁니다. 바뀌면 `space.updated`

### 메시지
- `message.list({space, channel, before?: EventId, limit?: number(≤200), thread?: EventId}) → {messages: MessageView[], gaps: GapView[]}`
  — `before`는 그 공간·채널의 메시지여야 함(아니면 `1004`). `thread`가 포럼 게시글이면 그 게시글 자체가 글타래의 첫 메시지로
  들어갑니다(쪽을 넘기다 처음에 닿을 때, 숨긴 게시글은 빼고). `limit`보다 적게 오면 그보다 오래된 메시지가 없다는 뜻입니다(신고하며
  숨긴 메시지를 뺀 자리는 더 오래된 메시지로 채움). `messages`는 `before`(없으면 맨 끝)보다 앞의 가장 최근 `limit`개를
  오래된 것부터 줍니다(일반 채널 (lamport, id), 휘발성 채널 (lamport, 작성 시각, id) 오름차순)
  - `MessageView = {id, author: UserId, author_device, created_at, lamport, text, attachments: AttachmentView[], embeds, reply_to?, reactions: {emoji, count, me}[], edited_at?, deleted: bool, volatile: bool, expires_at?, thread?, author_blocked: bool, anonymous?, anon_name?, mine?, mentions_me?}`
  - `author_blocked`: 내가 차단한 사용자의 메시지. 내용은 그대로 오고 화면이 접어서 보여 줍니다
  - `anonymous`: 익명 글(익명 채널에서 썼거나 그 채널이 지금 익명). `author`·`author_device`는 모두 0인 ID(`ANONYMOUS_USER`·
    `ANONYMOUS_DEVICE`, base58 `11111111111111111111111111111111`)이고 `author_blocked`는 늘 거짓. `mine`은 내가 쓴 글(익명 글 포함). 둘 다 거짓이면 빠짐.
    `mentions_me`는 나를 멘션한 글: `<@내 ID>`, 또는 그 채널에서 `mention_many` 권한이 있는 작성자의 `@everyone`, 작성 뒤 5분 안에
    받은 `@here`(받는 데몬이 작성자 권한으로 정하고 안 읽은 멘션 수에도 셈, 내 글은 아님). 거짓이면 빠짐.
    `anon_name`은 익명 글의 가명('형용사 + 동물', 예: `용감한 수달`): (공간, 채널, 사용자)로 정해져 같은 채널에서는 늘 같고 모든
    멤버 화면에서 같으며 채널이 다르면 다름(decisions.md "익명 가명"). 익명 글의 알림(`message.new`로 띄우는 OS 알림)은 보낸 사람이
    그 가명이고 기본 프사
  - 신고하며 숨긴 메시지(`message.report`의 `hide`)는 목록·검색에서 빠집니다
  - `GapView = {after?: EventId, before?: EventId}` — 받지 않은 기록 구간
- `message.send({space, channel, text, attachments?: AttachmentInput[], reply_to?, thread?}) → {id}`
  — 본문의 `<@사용자 ID>` 중 크루 멤버는 멘션으로 담습니다(100명까지). `@everyone`·`@here`는 글자 그대로 보내고 받는 쪽이
  작성자의 `mention_many` 권한을 보고 멘션으로 칩니다(앞뒤가 글자·숫자·`_`이거나 앞이 `\`면 아님, 위 `mentions_me`). 첨부는 10개까지, 경로는 절대 경로의 보통 파일(또는 `paste`), 이름은 1~255자. 하나의 크기는 기본 무제한이고 채널의
  `attach_limit`이 있으면 그것까지(넘으면 파일을 읽기 전에 `1005`). 휘발성 채널의 첨부는 아래.
  첨부에는 ATTACH_FILES가 필요(없으면 파일을 읽기 전에 `1003`). 이미지 첨부는 썸네일(`thumbnail_base64`, 긴 변 320px 이하)과
  `width`·`height`를, MP4·MOV는 `duration_ms`를 보내는 쪽이 채웁니다. 보내기가 실패하면 준비한 첨부 블롭은 지웁니다.
  - `AttachmentInput = {path, paste?, name?, thumbnail_base64?, width?, height?, duration_ms?, keep_metadata?}` — `keep_metadata`가
    참이 아니면(기본) 데몬이 JPEG·PNG·WebP·GIF·MP4·MOV의 메타데이터(촬영 위치·기기·시각 등)를 지운 내용을 보내고, 첨부 크기·썸네일도
    지운 내용 기준입니다(회전값은 남김, 다른 형식은 그대로, decisions.md "첨부 메타데이터"). 구조가 깨져 지우지 못하면 `1005`. `paste`는 `blob.paste`로 미리 올린
    붙여넣기 ID로, 주면 `path`는 비우고 `name`이 꼭 있어야 함(없거나 만료된 ID `1004`, 다 올리지 않았으면 `1005`). 보내기는 붙여넣기를
    빌려 쓰기만 하고 지우지 않음(`blob.paste_discard`). `thumbnail_base64`부터 뒤의 넷은 데몬이 만들 수 없는 동영상·음성의
    미디어 힌트로, 앱(웹뷰의 `<video>`·`<audio>`)이 만들어 넘깁니다. 썸네일은 PNG·JPEG·WebP, 64KiB·긴 변 640px 이하로 끝까지 풀리는
    것만 받고 데몬이 긴 변 320px JPEG로 다시 만들어 싣습니다. `width`·`height`는 함께, 각각 1~16384이고 음성이면 버립니다.
    `duration_ms`는 1밀리초~7일이며, 데몬이 파일에서 길이를 읽을 수 있으면(MP4·MOV) 그 값이 이깁니다. 힌트는 있으면 좋은 정보라
    형식이 맞지 않아도 보내기를 막지 않습니다: 파일 내용이 이미지면 무시하고(데몬이 직접 만듦), 동영상·음성으로 가리지 못한 파일이면
    힌트만 버리고 보냅니다(앱과 데몬의 형식 판별이 다를 수 있음. 데몬은 ID3·머리말 없는 MP3·AAC(ADTS)도 음성으로 가림). 깨진
    base64, PNG·JPEG·WebP가 아니거나 너무 큰 썸네일, 한쪽만 준 가로·세로, 위 범위를 벗어난 값은 `1005`(첨부 블롭을 만들기 전).
    올리는 첨부는 등록하자마자 고정해 두어, 암호화하는 동안 저장 공간 정리가 비우지 못합니다
  채널의 `link_previews`가 켜져 있고 EMBED_LINKS가 있으면 보내는 쪽이 본문 링크의 미리보기를 만들어 함께 암호화해 보냅니다(받는
  쪽은 조회하지 않음. 없으면 미리보기 없이 보냄).
  보내기와 목록 보기는 그 채널을 "열어 본 채널"로 기록합니다(시더 기본 범위·미룬 자동 내려받기·휴면 깨우기)
  휘발성 채널: `thread`는 `1005`, 링크 미리보기는 채널 설정과 상관없이 만들지 않음, 들어가 있지 않거나 상시 참여 상한이 차서 밀려났으면 `1010`, 세션 키를 10초 안에
  받지 못하면 `1010`. 첨부는 데몬 메모리에만 두고(디스크·임시 파일 없음) 메시지와 함께 지웁니다. 하나가 채널의 `attach_limit`이나
  `--volatile-attachment-mib`(기본 무제한)를 넘으면 `1005`, 이 노드의 휘발성 첨부 RAM 예산 `--volatile-ram-mib`(기본 1GiB)가 모자라면 파일을 읽기 전에 `1010`.
  첨부를 메시지에 묶지 못하면(그새 채널을 나감 등) 메시지를 보내지 않고 `1010`(이 기기에 잠깐 보였던 메시지는 `message.expired`)
- 휘발성 채널의 `message.list`는 RAM에 있는 것만(참가 중이 아니면 빈 목록, `gaps` 없음), `volatile: true`, 수명이 있으면 `expires_at`.
  자동화 토큰은 `channel.allow_api_read` 전까지 `1007`
- `message.edit({space, channel, id, text}) → {}` — 수정 금지 채널이면 `1003`
- `message.delete({space, channel, id}) → {}` — 남의 메시지는 MANAGE_MESSAGES(AI면 사람 확인). 삭제 금지 채널에서는
  MANAGE_MESSAGES가 있어야 함(자기 메시지도, 아니면 `1003`)
- `message.react({space, channel, id, emoji}) → {}` / `message.unreact({...}) → {}` — emoji는 유니코드 또는 `<:이름:ID>`
- `message.search({space?, channel?, author?, query?, offset?, limit?}) → {messages: MessageView[], total, offset, took_ms}` (read) —
  일반 등급만(로컬 DB, 채팅·포럼 게시글·댓글). 결과 수에 상한이 없고 최신순(작성 시각, 같으면 ID 내림차순)으로 `offset`부터
  `limit`개(기본 50, 최대 500)씩 쪽을 나눔. `total`은 권한·숨김까지 거른 전체 수, `offset`은 실제 시작 위치(끝을 넘으면 마지막 쪽),
  `took_ms`는 데몬에서 걸린 시간. `messages`는 `space`·`channel`을 채움.
  - 검색어: 빈칸으로 나눈 낱말이 모두 들어 있는 글(순서 무관). 큰따옴표(`"…"`, `“…”`)로 묶으면 빈칸까지 구절 그대로. 대소문자·전각
    (NFKC)·연속 빈칸·줄바꿈 차이는 무시. 본문·포럼 제목·첨부 파일 이름에서 찾고, 칸(본문·제목·이름)을 넘는 일치는 없음
  - `author`를 주면 그 사람이 쓴 글만(이때는 `query`가 비어도 됨). 익명으로 쓴 글과 지금 익명인 채널의 글은 빠짐(검색으로
    익명 글쓴이를 알아낼 수 없게). `author` 없이 `query`가 비면 빈 쪽(`total: 0`)
  - 첫 검색은 이 기기의 일반 등급 기록을 한 번 복호화해 RAM 색인을 만드느라 기록 크기만큼 걸림(그동안 다른 호출은 막히지
    않음, 블로킹 스레드). 그 뒤로는 바뀐 메시지만 맞추므로 쪽마다 다시 찾아도 빠름. 색인은 디스크·로그에 남지 않고, 지운
    메시지는 지운 직후 색인에서도 덮어 지움
- `forum.create_post({space, channel, title, text?, tags?, attachments?, featured?}) → {id}` — `create_posts`. 태그는 채널에 있는 것,
  5개까지. 본문(마크다운 원문 그대로, decisions.md "마크다운")과 첨부 중 하나는 있어야 함(`1005`). `attachments`는 `message.send`와 같고 `attach_files`가
  필요, 링크 미리보기도 `message.send`와 같음. `featured: true`면 올린 뒤 대문으로 정함(그 채널의 `manage_channels`). 권한은 파일을
  읽기 전에 모두 확인하고(`1003`), 게시글을 내지 못하면 준비한 첨부를 버림.
  포럼 채널에서 `thread` 없는 `message.send`는 `1005`(댓글은 `thread: 게시글 ID`)
- `forum.list_posts({space, channel, sort?: "activity" | "created", order?: "desc" | "asc", tags?, tag_match?: "any" | "all", tag?}) → ForumPostView[]`
  — 지운 글 빼고 200개까지. 고정한 글이 먼저, 그다음 `sort`(마지막 활동 시각 또는 처음 쓴 순서)를 `order`대로(기본 새것부터).
  `tags`가 있으면 `tag_match`(기본 `any`: 고른 태그가 하나라도 있는 글, `all`: 글의 태그가 고른 태그와 꼭 같은 글)로 거름.
  예전 `tag`는 `tags`에 더해짐.
  익명 게시글은 `anonymous: true`, `author`가 `ANONYMOUS_USER`, `anon_name`에 가명(`MessageView`와 같음), 내 글이면 `mine: true`
  - `ForumPostView = {id, title, tags, author, created_at, last_activity, reply_count, pinned, locked, anonymous?, anon_name?, mine?, featured?, edited_at?, preview, cover?, attachment_count}`
  - `edited_at`: 게시글 본문(`message.edit`)이나 제목·태그(`forum.update_post`, 고정·잠금은 아님)를 마지막으로 고친 때(고친 적
    없으면 빠짐). 제목·태그는 그 변경 이벤트의 작성 시각입니다
  - `featured`: 이 채널의 대문 글. `preview`: 본문 앞 200자(마크다운 그대로). `cover`: 첨부 중 첫 이미지·영상(`AttachmentView`,
    갤러리 보기의 표지). 본문 전체와 첨부는 `message.get`·`message.list({thread})`의 게시글 메시지로 받음
- `forum.get_post({space, channel, post}) → ForumPostView` (read) — 목록 항목 하나. 없거나 지운 글 `1004`
- `forum.set_featured({space, channel, post: EventId | null}) → {}` (write) — 그 채널의 `manage_channels`(`1003`), 포럼 채널만(`1005`),
  이 기기에 있는 그 채널의 게시글만(`1004`). `null`은 대문 내리기. 지금과 같으면 아무것도 내지 않고 `{}`.
  대문 글을 지우면 대문은 비어 보임(protocol.md "포럼 대문")
- `forum.update_post({space, channel, post, title?, tags?, pinned?, locked?}) → {}` — `pinned`·`locked`는 `manage_messages`,
  `title`·`tags`는 작성자나 `manage_messages`. 볼 수 없는 멤버가 있는 채널의 제목·태그 변경은 거절(`1005`, 변경은 멤버 전원이
  읽는 상태 레인으로 가기 때문). 변경마다 바뀐 항목만 싣고, 항목마다 (Lamport, 이벤트 ID)가 큰 것이 이김(게시글보다 먼저 와도 보관했다가 적용).
  잠긴 글에는 `manage_messages` 없이 댓글을 달 수 없음

### 차단·신고
- `user.block({user}) → {}` (write) — 이 기기에만 저장하고 상대에게 알리지 않습니다. 함께 있는 크루에서 나가게 하지 않습니다.
  차단하면: 그 사람 메시지에 `author_blocked: true`, 그 사람의 `message.new` 알림을 보내지 않고 안 읽은 수에서 뺌, 친구 요청을
  받지 않음(링크 사용·응답은 차단하지 않았을 때와 같고 요청·알림만 남지 않음, 오가던 요청은 지움), 첨부를 자동으로 받지 않음,
  시더로서 메시지·첨부·이모지를 주지 않음(제공 범위 밖과 같은 응답). 자기 자신 `1005`, 10,000명까지. 이미 차단했으면 그대로 `{}`
- `user.unblock({user}) → {}` (write) — 차단하지 않은 사용자여도 `{}`
- `user.blocked() → {user, profile: Profile | null, blocked_at}[]` (read) — 최근 것부터
- `user.devices({user}) → UserDeviceView[]` (read) — 이 기기가 아는 그 사람의 기기(서명 기기 목록, 폐기한 것은 뺌):
  `{device, name, added_at, blocked}`. 모르는 사람이면 빈 목록
- `user.block_device({user, device}) → {}` (write) — 남의 계정에서 기기 하나만 이 기기에서 차단합니다(친구 계정의 낯선
  기기를 끊는 용도). 그 기기의 인증된 연결을 잊고 다시 인증을 걸면 거절하며(이 노드와 주고받지 않음), 그 기기가 쓴 메시지에
  `author_blocked: true`, `message.new` 알림·첨부 자동 받기·입력 표시를 막습니다.
  상대에게 알리지 않습니다. 키 그룹은 바꾸지 않으므로 그 기기가 이미 받은 키·기록과 다른 노드가 전하는 것은 막지 못합니다(그
  계정의 활성 기기가 `device.revoke`해야 끝남). 안 읽은 수는 사람 단위라 그대로 셉니다. 내 기기면 `1005`(폐기를 씀), 10,000대까지,
  이미 차단했으면 그대로 `{}`
- `user.unblock_device({user, device}) → {}` (write) — 차단하지 않은 기기여도 `{}`
- `user.blocked_devices() → {user, device, name, profile: Profile | null, blocked_at}[]` (read) — 최근 것부터
- `message.report({space, channel, message, reason, hide?}) → {report_id}` (write) — 크루만(1대1은 `1005`, `user.block`을 씀),
  채널을 볼 수 있어야 함(`1003`). `reason`은 필수, 512자까지. 이 기기가 가진 메시지만(`1004`), 자기 메시지는 `1005`.
  그 채널을 볼 수 있는 운영진(`kick`·`ban`·`manage_messages` 중 하나라도 있는 멤버)의 활성 기기마다 봉인해 직접 보냅니다(다른
  멤버는 신고가 있었는지 모름, 메시지 작성자가 운영진이어도 그 사람에게는 안 감). 받을 운영진이 없으면 `1005`. 오프라인이거나
  아직 상태가 따라오지 않은 운영진, 신고가 많이 쌓인 운영진에게는 나중에 다시 보냅니다(30일까지). 같은 메시지를 다시 신고하면 앞의 `report_id`를 돌려줍니다. `hide: true`면 이 기기에서 그 메시지를 숨기고 시딩에서도
  뺍니다(숨김 해제 API는 아직 없음). 휘발성 채널 메시지의 신고·숨김은 RAM에만
- `report.list({space, status?: "open" | "resolved"}) → ReportView[]` (admin) — 운영진만(아니면 `1003`), 이 기기가 받은 신고, 최근 것부터.
  `ReportView = {report_id, space, channel, message, reporter, reason, reported_at, status: "open" | "resolved", resolution: string | null}`
- `report.resolve({space, report_id, action: "dismiss" | "delete_message" | "kick" | "ban", note?}) → {}` (admin) — 운영진만.
  `dismiss`가 아니면 AI는 사람 확인(`1006`, 휘발성 채널 신고면 확인 요청도 RAM에만). 없는 신고 `1004`, 이미 처리함 `1010`. `dismiss`가 아니면 작성자를 신고 내용이 아니라
  이 기기의 메시지 사본으로 확인하고(사본이 없으면 `1004`), `delete_message`는 `message.delete`, `kick`은 `member.kick`(이미 나갔으면
  건너뜀), `ban`은 `member.ban`(이미 밴됐으면 건너뜀)과 같은 권한 검사를 거칩니다. 처리 결과(`resolution` = 처리 방법,
  메모가 있으면 `"<방법>: <메모>"`, 메모는 512자까지)는 다른 운영진 기기에만 보냅니다(신고당한 사람은 운영진이어도 빼고,
  신고한 사람은 운영진일 때만 받음). 메모는 추방·밴 사유로 쓰지 않습니다(상태는 멤버 모두가 읽으므로)

### 첨부·이모지·블롭
블롭은 그 공간에서 쓰이는 것(첨부를 실은 메시지, 공간 이모지·아이콘, 멤버 프로필 이미지)만 다룹니다. 아니면 `1004`.
다른 기기에 내줄 때는 더 엄격합니다(그 공간에 등록돼 있고 키가 맞는 쓰임만, protocol.md "파일 스웜").
채널의 `attach_limit`을 넘는 첨부(한도를 지키지 않는 클라이언트가 보낸 것)는 자동으로 받지 않습니다(`blob.fetch`로 직접 받기는 됨).

휘발성 채널의 첨부(메시지 보기가 `volatile: true`)는 데몬 RAM에만 있고 같은 메서드로 다룹니다. 다른 점:
- 실은 메시지가 사라지면(만료·삭제·퇴장·종료) 첨부도 사라져 `1004`. 자동화 토큰은 그 채널의 `channel.allow_api_read` 전까지 `1007`.
- 받기는 지금 참가자에게서, 메시지가 살아 있는 동안만. 자동 받기는 피어 설정(종류·자동 한도)을 따르고 차단한 사용자의 첨부는
  받지 않습니다. `blob.fetch`는 이 기기의 하나 한도(`--volatile-attachment-mib`, 기본 무제한, 채널 한도는 보지 않음)를 넘으면 `1005`, RAM 예산(`--volatile-ram-mib`)이 모자라면
  `1010`(받는 중인 휘발성 첨부가 노드 대기열 상한 256개를 채웠을 때도 `1010`). 가진 참가자가 없으면 1초부터 10초 간격으로 다시
  시도하다 10분 동안 하나도 받지 못하면 멈추고 받다 만 조각도 지웁니다(다시 `blob.fetch`하면 처음부터). 다시 시도를 기다리는
  동안은 가진 만큼만 예산을 잡습니다. 동시 내려받기 수(Tor로 돌면 1, P2P 연결로만 받는 첨부는 따로)와 `download_limit_bps`는 일반 첨부와 나눠 씁니다.
  `blob.status`의 `seeders_online`은 지금 참가자 중 가진 기기 수(기억하지 않음).
- 보낼 때 `AttachmentInput`의 미디어 힌트(`thumbnail_base64`·`width`·`height`·`duration_ms`)는 무시합니다(이미지 썸네일은
  데몬이 메모리에서 만듦, 앱은 휘발성 채널에서 동영상·음성을 미리 읽지 않음).
- `blob.read`·`blob.save`의 `key_from`은 그 첨부를 실은 메시지여야 합니다(아니면 `1004`).
- `blob.save`는 소유자 토큰이 `allow_volatile: true`를 줄 때만 씁니다(아니면 `1010`, 다른 토큰은 경로를 넘기지 못해 `1002`). 저장한 파일은 메시지가
  사라져도 남습니다.
- 저장 공간 정리·고정(아래)은 디스크의 블롭만 다룹니다. 휘발성 첨부는 `blob.status`의 `pinned`·`kept`가 늘 `false`이고
  `blob.pin`은 `1004`입니다. 받은 크기는 위 RAM 예산으로만 셉니다.

- `blob.status({space, blob}) → {have_chunks, total_chunks, seeders_online, size, downloading, pinned, kept, volatile}`
  — 매니페스트를 아직 받지 못했으면 `total_chunks = 0`. `size`는 평문 크기(매니페스트가 없으면 메시지에 적힌 크기),
  `downloading`은 내려받기 대기열에 있거나 받는 중. `seeders_online`은 인증된 멤버 기기에 `BlobHave`로 물어 하나라도 가진
  기기 수(2초 제한, 10초 동안 기억). `pinned`는 `blob.pin`으로 고정한 블롭, `kept`는 저장 공간 정리에서 늘 빼는 블롭(내가
  올린 첨부·이모지·공간 아이콘·프로필 이미지). 정리로 비운 블롭은 `have_chunks = 0`(매니페스트는 남아 다시 받을 수 있음).
  `volatile`은 휘발성 채널 첨부(RAM 전용)이며, `blob.read`로 받아 파일에 쓰는 클라이언트는 `blob.save`의 `allow_volatile`처럼
  사람에게 확인받아야 합니다(CLI `blob read -o 파일 --allow-volatile`)
- `blob.fetch({space, blob, priority?}) → {}` — 수동 다운로드. 자동 다운로드 한도·휴면과 무관하게 받고 포기하지 않습니다(다시 시도
  간격 5초→최대 5분, 다시 부르면 바로 시도). 받기 제한이 0(피어 설정 `download_limit_bps`, 크루·채널 `rate.set_limit`)이면
  `1003`(채널에 속하지 않는 이미지는 조용히 `{}`). `priority` 기본 128(클수록 먼저, 보고 있는 채널이 먼저). 채널에 속하지 않는
  이미지(프로필 사진·공간 아이콘·이모지, 앱이 그리다 못 읽은 것)는 자동 받기 줄에 넣어 자동 다운로드처럼 몇 번 실패하면
  포기합니다(아무도 갖지 않은 사진이 받기 자리를 계속 차지하지 않게)
- `blob.read({space, blob, key_from: EventId, index?, offset?, len?, binary?: bool}) → {data_base64?} | 바이너리 프레임`
  — 내용(원래 파일, 압축 머리글을 벗긴 것)을 돌려줍니다. `key_from`은 그 첨부를 실은 메시지(이모지·프로필 이미지는 키를 따로
  찾으므로 무시). `index`면 내용을 조각 평문 크기(조각 크기 − 16)로 나눈 그 번째 부분, 아니면 `offset`(기본 0)부터 `len`(기본 끝까지,
  최대 8MiB)바이트. 첫 조각(머리글이 든 곳)이나 걸린 조각을 아직 받지 않았으면 `1010`. 압축한 블롭(protocol.md "파일 스웜")은
  다 받아야 읽힙니다.
  `binary: true`면 결과가 종류 `2` 프레임(요청 ID가 음이 아닌 정수여야 함)
- `blob.save({space, blob, key_from, path, allow_volatile?}) → {}` — 다 받은 블롭만(아니면 `1010`), `path`는 절대 경로, 있는 파일은 덮어쓰지
  않음(`1010`). 실패하면 만들던 파일을 지웁니다. Windows에서는 저장한 파일에 "인터넷에서 받음" 표시(`:Zone.Identifier`,
  `ZoneId=3`, 주소 없음)를 붙여 Office 매크로 차단·SmartScreen이 검사하게 합니다(지원하지 않는 드라이브면 건너뜀)
- 저장 공간 정리: 남에게서 받은 첨부(정리 대상)는 한도(기본 2GiB)를 넘거나 데이터 경로 디스크의 여유가 최소 여유(기본 1GiB)보다
  적으면 오래 안 쓴 것(마지막으로 받거나 읽거나 내준 시각)부터 조각 데이터를 비웁니다. 고정한 블롭, 내가 올린 첨부, 이모지·공간
  아이콘·프로필 이미지는 비우지 않고(썸네일은 메시지에 실려 늘 남음), 받는 중·받을 차례이거나 30초 안에 내주거나 읽은 블롭은
  건너뜁니다. 비운 블롭은 등록·매니페스트가 남아 `blob.fetch`·자동 다운로드로 다시 받을 수 있고, 남에게는 그 블롭을 더는 가지지
  않은 것으로 답합니다. 자동 다운로드는 시작 전에 자리를 만들고, 만들 수 없으면 받지 않습니다(수동 다운로드·고정은 그대로 받음)
- `blob.cache_status() → BlobCacheView` (read) — `{cache_bytes, cache_count, pinned_bytes, pinned_count, kept_bytes, kept_count,
  limit_bytes, min_free_bytes, default_limit_bytes, default_min_free_bytes, disk_free_bytes, evicted_count, evicted_bytes}`.
  바이트는 디스크에 있는 암호문 조각 기준, `disk_free_bytes`는 모르면 `null`, `evicted_*`는 데몬이 켜진 뒤 비운 양.
  `default_*`는 `yangpad --cache-limit`·`--cache-min-free`(없으면 2GiB·1GiB)
- `blob.cache_set_limit({limit_bytes?, min_free_bytes?, reset?}) → BlobCacheView` (settings, 줄이면 AI는 사람 확인) — 빠진 값은
  그대로, 바꾸면 바로 정리. `reset: true`면 둘 다 기본값으로(값과 함께 주면 `1005`). 한도 0이면 자동 다운로드를 하지 않고, 직접
  받은 첨부도 다 쓴 뒤 정리 주기에 비웁니다. 한도를 낮추거나 최소 여유를 올리면(기본값으로 되돌려 그렇게 되는 경우 포함) 받은
  첨부를 모두 비울 수 있어 자동화 토큰이면 `1006`(사람 확인), 늘리거나 그대로면 바로 실행
- `blob.cache_clear() → {evicted_count, freed_bytes}` (settings, AI면 사람 확인) — 정리 대상을 지금 모두 비웁니다(건너뛰는 블롭은 위와 같음)
- `blob.pin({space, blob}) → {}` / `blob.unpin({space, blob}) → {}` (write) — 고정은 저장 공간 정리에서 빼고, 아직 받지 않았으면
  받기 시작합니다(`blob.fetch`와 같음). 이 공간에서 쓰이는 블롭만(`1004`, 휘발성 채널의 첨부는 해당 없음), 이미 고정했으면 그대로
  `{}`. 풀기는 그 공간에서 고정한 것만(`1004`). 고정을 풀어도 내가 올린 첨부는 계속 남깁니다
- `blob.pins({space?}) → {space, blob, pinned_at, size, downloaded}[]` (read) — 고정한 블롭. 토큰의 공간 범위 밖은 빠집니다
- `blob.evict({space, blob}) → {}` (write, 파괴적: 자동화 토큰은 사람 확인) — 이 기기에 둔 첨부 데이터만 지웁니다(라이브러리 탭의
  "이 기기에서 지우기"). 메시지·등록·매니페스트는 남아 `blob.fetch`로 다시 받을 수 있고, 고정도 풉니다. 내가 올린 첨부도 지울 수
  있으며 그러면 이 기기는 더는 내주지 못합니다(받아 둔 기기가 없으면 다시 받을 수 없음). 이 공간 일반 채널의 첨부만(`1004`,
  휘발성 첨부는 기록되지 않아 여기 해당), 이모지·공간 아이콘·프로필 이미지로도 쓰이는 블롭은 `1005`. 받는 중이면 받기를 멈추고
  `1010`(늦게 온 조각이 다시 남지 않게, 잠시 뒤 다시 부름). 블롭 데이터는 기기에 하나라 같은 블롭을 실은 다른 메시지도 함께 비워집니다
- `library.list({space?, messages?, offset?, limit?}) → {items: LibraryItem[], total}` (read) — 이 기기가 보내거나 받은 첨부를 실은
  메시지(라이브러리 탭). 첨부 쓰임 기록(`blob.ref.*`)에서 모으므로 기록 전체를 풀지 않습니다. 일반 등급 채널만(휘발성 채널의 첨부는
  기록이 없어 빠짐). 거르기는 채팅 목록과 같음: 토큰의 공간 범위, 멤버, 채널 보기 권한, 들어가 있는 동안만 보이는 채널, 이 기기가
  참여하지 않은 P2P 전용 채널, 지운 글, 신고하며 숨긴 글, 차단한 사람의 글. 익명 글은 글쓴이를 가립니다(`message.list`와 같음).
  `messages`가 없으면 전체를 최신순(논리 시계) 쪽으로 나눠 첨부 썸네일 없이(`limit` 기본·최대 500, `offset`이 `total`에 닿을 때까지
  읽음, 지운 글 등이 빠져 쪽이 짧을 수 있음), 있으면 그 메시지만(앞의 200개) 썸네일과 함께(갤러리가 보이는 칸만 읽음).
  `LibraryItem = {message: MessageView(space·channel 채움), received_at, excluded}`. `received_at`은 이 기기가 그 메시지를 받은 시각
  (이 기기에서 쓴 글이면 만든 시각), `excluded`는 `seed.exclude_message`로 공유를 멈춘 메시지
- `blob.relink({space, channel, message, blob, path}) → BlobStatusView` (write) — 이 기기에 데이터가 빠진 첨부(저장 공간 정리로 비움,
  새 기기·다시 설치)를 같은 내용의 로컬 파일(절대 경로)로 다시 채웁니다. 일반(휘발성이 아닌) 채널의 그 메시지에 달린 첨부만
  (휘발성 `1005`, 메시지·첨부가 없으면 `1004`). 첨부의 키와 같은 조각 크기로 파일을 다시 암호화해 블롭 ID(조각 해시들의 해시)가
  같을 때만 받아들이고, 크기나 내용이 다르면 아무것도 쓰지 않고 `1005`("해시가 다름"). 파일 그대로 맞지 않으면 보낼 때처럼
  메타데이터를 지운 내용으로 한 번 더 맞춰 봅니다(지우고 보낸 첨부도 원본 파일로 다시 이음). 받아들이면 받지 않고 바로 다 차고, 보관(고정)
  하며 `blob.progress`를 보냅니다. 파일 경로는 기억하지 않습니다. 데몬은 늘 암호화해 둔 사본으로 내주므로 원본 파일을 옮기거나 지워도
  시딩은 멈추지 않습니다(사본이 없어졌을 때 쓰는 기능)
- `blob.paste({paste?, size?, data_base64}) → {paste, received}` (write) — 클립보드에서 붙여넣은 이미지·파일처럼 경로가 없는 첨부를
  데몬 RAM에 미리 올립니다(보낼 때 `AttachmentInput.paste`). 처음에는 `paste` 없이 전체 크기 `size`(0은 `1005`)와 첫 조각을 주고,
  돌려받은 ID로 나머지를 이어 붙입니다. 조각은 4MiB 이하, `received`는 지금까지 받은 바이트. 처음에 `size`만큼 잡은 버퍼에만 두고
  지울 때 0으로 덮어씀(디스크·임시 파일 없음, 휘발성 채널에도 씀). 알린 크기를 넘으면 그 붙여넣기를 지우고 `1005`, 보내는 중인
  붙여넣기에 이어 붙이면 `1010`, 없거나 만료된 ID `1004`. 모두 합쳐 512MiB·64개까지(넘으면 `1005`), 마지막으로 이어 붙인 뒤
  30분이 지나면 지움. 신원이 있어야 함
- `blob.paste_discard({paste}) → {}` (write) — 붙여넣기를 지웁니다(보냈거나 첨부에서 뺐을 때). 없는 ID도 그대로 `{}`
- `emoji.list({space}) → EmojiView[]` / `emoji.create({space, name, path}) → {id, animated}` / `emoji.delete({space, id}) → {}`
  — `manage_emoji`. PNG·GIF·WebP(크기 제한 없음, 크루가 `emoji_limits.max_bytes`를 정했으면 그 이하, 애니메이션은 파일
  머리로 판정), 이름 2~32자 글자·숫자·밑줄, 개수는 `emoji_limits.max_count`(없으면 500)까지(넘으면 `1005`). ID는 이미지 해시라
  같은 이미지는 두 번 못 올림(`1010`). 메타데이터는 늘 지워 올리고 ID도 지운 이미지의 해시(깨져 지우지 못하면 `1005`). `EmojiView.created_by`는 올린 사용자(이 판 전의 상태 캐시에서 온 이모지면 없음)

### 역할·초대
- `role.list({space}) → Role[]` — 수준 높은 순. 역할의 수준(`Role::level`)은 `administrator` 권한이 있으면 2³²를 더한 `rank`라,
  관리자 권한 역할은 `rank`와 상관없이 일반 역할보다 위입니다. 멤버의 순위는 가진 역할 수준의 최댓값(소유자는 가장 위)이고,
  역할·멤버를 다루는 모든 순위 비교(추방·밴·역할 부여·초대 역할 포함)가 이것을 씁니다
- `role.create({space, name, color?, permissions: string[], rank?}) → {role}` (admin) — `manage_roles`. 권한 이름:
  `view_channel`, `send_messages`, `read_history`, `add_reactions`, `attach_files`, `embed_links`, `use_external_emoji`,
  `connect_voice`, `speak`, `stream`, `create_posts`, `use_crew_stun`, `mention_many`, `manage_messages`, `manage_channels`, `manage_roles`, `kick`,
  `create_invite`, `manage_emoji`, `manage_voice`, `manage_space`, `ban`, `administrator`. `mention_many`는 20명 넘는 멘션과
  `@everyone`·`@here`. 자기에게 없는 권한은 못 줌,
  `rank`(기본 1)는 1 이상(`1005`)이고 만들 역할의 수준이 내 순위보다 낮아야 함(`1003`: 관리자 권한 역할을 만들면 관리자 역할
  사이의 순위로 봄). `color`는 `0xRRGGBB`
- `role.update({space, role, name?, color?, permissions?, rank?, visible_to?, see_admins?}) → {}` (admin, AI면 사람 확인) — @everyone은
  권한·색·`see_admins`만. 표시 설정(화면에서만 거름, 상태 이벤트 `RoleDisplaySet`): `visible_to`는 이 역할을 볼 수 있는 역할
  (`null`이면 모든 멤버, 목록이면 그 역할을 가진 사람만. 역할을 가진 사람과 역할 관리자는 늘 봄, @everyone을 넣으면 `null`과 같음,
  @everyone 역할에는 `1005`). `see_admins`는 이 역할을 가진 사람이 관리자(소유자·관리자 권한 역할을 가진 멤버)를 보는지로, 권한처럼
  더해집니다(내 역할 중 하나라도 켜져 있으면 봄). 기본은 @everyone만 켬(모두가 봄)이라, 관리자를 가리려면 @everyone에서 끄고
  볼 역할에서 켭니다
- `role.delete({space, role}) → {}` (admin, AI면 사람 확인)
- `invite.create({space, expires_in_secs?, max_uses?, roles?}) → {url}` (AI면 사람 확인) — 크루만, `create_invite`. 0은
  `1005`. `expires_in_secs`를 주지 않으면 7일 뒤 만료(만료 없는 초대는 없음), `max_uses`가 없으면 횟수 제한 없음.
  `roles`는 `manage_roles`와 내 순위보다 낮은 역할만. 링크에는 크루 이름을 넣지 않고, 상태에는 토큰 해시와 토큰에서 만든
  검증 공개키만 남음(가입 이벤트에도 토큰 대신 그 키로 한 서명만 실림). 링크(`https://dhtpdud.github.io/YANGPA/#invite/…`, 읽을 때는 `yangpa://invite/…`도 받음, decisions.md "링크 형식·웹 초대 페이지")는 2판이고, 크루에 상태
  체크포인트가 있으면 만들 때의 최신 체크포인트(순번, ID)를 고정해 약 40B 길어집니다(protocol.md "상태 체크포인트"). 1판 링크는
  `invite.join`이 `1005`로 거절하므로 이 판 이전에 만든 링크는 다시 만들어야 합니다
- `invite.list({space}) → InviteView[]` / `invite.revoke({space, token_hash}) → {}` — `create_invite`가 없으면 자기가 만든
  초대만 보이고 그것만 폐기
- `invite.join({url}) → {space, status: "pending" | "joined"}` — 링크 주인 노드에 `RedeemInvite`. `joined`는 멤버가 되고
  크루 키 그룹 키를 받은 것, `pending`은 확정 기기의 가입 키 갱신(받은편지함 키 묶음)을 기다리는 중(데몬이 다시 따라잡음, 그동안
  `space.list`에 없을 수 있음). 가입 묶음에는 체크포인트로 가입했으면 그 체크포인트에서 유효한 상태 키(`valid_state_keys`)와 그 뒤
  상태 키 가운데 최근 64개가, 체크포인트가 없으면 최근 상태 키 64개가 실립니다. 그보다 오래된 키로 쓴 상태 이벤트(체크포인트로 가입했으면 경계 F
  이상인 것만)가 있으면 멤버에게 그 키를 물어(`GetStateKeys`) 받는 동안 5초까지 기다렸다 답합니다(그 안에 못 받으면 `pending`).
  만료·폐기·횟수 초과·주인이 멤버가 아님은 `1005`, 닿지 못하면 `1009`(같은 링크로 다시 시도). 이미 멤버면 지금 상태를 돌려줌.
  받아 준 노드에 체크포인트가 있으면 데몬이 사슬(링크의 고정에서)과 스냅숏을 받아 그 체크포인트로 상태를 세우므로 공간 스트림
  전체를 받지 않습니다. 사슬이 링크의 고정에서 이어지지 않으면 경고를 남기고 처음부터 받습니다

### 동기화 설정
- `seed.get({space?}) → SeederPolicy` / `seed.set({space?, enabled?, types?, channels?, all_channels?, exclude_channels?, upload_limit_bps?, max_concurrent?, prefetch?}) → {}`
  — 빠진 필드는 그대로. `channels: null`이면 기본 범위(본인이 연 채널만, `all_channels: true`면 열지 않은 채널까지), 목록이면 정확히
  그 채널들. `prefetch: true`(미리 받기, 헤드리스 시더)면 제공 범위 채널의 첨부를 열지 않아도 받아 두고 내줍니다(목록이 없으면 열지
  않은 채널도 제공 범위가 됨). 내가 볼 수 있는 휘발성이 아닌 채널만, 피어 설정(종류·자동 다운로드 크기)과 저장 공간 한도 안에서.
  새 메시지(작성 시각이 10분 안)의 첨부는 바로, 지난 메시지(따라잡기로 들어온 것 포함)의 첨부는 정리 주기마다(그리고 켤 때) 한 번도
  받지 않은 것을 최근 것부터 남은 자리만큼 받으며, 자리를 만들려고 다른 첨부를 비우지 않습니다. 켜 둔 크루는 휴면하지 않습니다. `upload_limit_bps: null`은 무제한, 0은 아무것도 내주지 않음. 공간 값이 있으면 전역 값을 통째로 대신합니다. 전역 송신 상한·동시 전송 수를
  바꾸면 공간 값 중 이전 전역 값과 같은 것도 함께 바뀝니다(크루 화면의 스위치가 그때의 설정을 통째로 복사하므로, 따로 정하지 않은
  값은 전역을 따르게). 예전 판의 기본값(송신 2MiB/s, Tor 256KiB/s; Tor 수신 512KiB/s)이 그대로 저장된 설정은 처음 시작할 때 한
  번 새 값으로 옮깁니다. 기본값: 켜짐, 모든 종류, 송신 10MiB/s, 동시 4개(Tor로 돌면 동시 2개). `max_concurrent`는 동시에 보내 줄 기기 수입니다(한 기기가 겹쳐 보낸 요청은 하나로 셈). 앱 사용자 기기는 늘 Tor라 Tor 값이 기본이고, 직접 연결로 띄운
  데몬(`--direct`)만 앞의 값입니다
- `seed.exclude_message({space, channel, id, excluded}) → {}` — 그 메시지와 첨부를 제공하지 않음(공간마다 10,000개까지)
- `peer.get({space?}) → PeerPolicy` / `peer.set({space?, types?, priorities?, download_limit_bps?, auto_download_max_bytes?}) → {}`
  — `download_limit_bps: null`은 무제한, 0은 첨부를 받지 않음(메시지 동기화는 그대로). 기본값: 모든 종류, 수신 10MiB/s, 자동 다운로드 20MiB 이하(Tor로 돌면 2MiB). 프로필 이미지·공간 아이콘은 자동 다운로드 크기와
  상관없이 이미지 종류만 보고 받습니다(가져올 때 8MiB로 묶임). 우선순위 기본 텍스트 250, 이모지 200,
  이미지 150, 영상 100, 파일 50, 기록 묶음 20. 공간 값을 두면 그 공간의 수신 대역폭을 따로 셉니다
- `rate.limits() → RateLimitView[]` (read) / `rate.set_limit({scope, up_bps?, down_bps?}) → {}` (settings) — 크루·채널·사람별 올리기·받기
  제한(이 기기만의 설정, 전체 제한과 따로). `scope`: `{kind: "crew", space}` | `{kind: "channel", space, channel}` | `{kind: "user", user}`.
  `RateLimitView = {scope, up_bps, down_bps}`(바이트/초, `null`은 그 방향 제한 없음). `set_limit`은 그 범위의 값을 통째로 바꾸고, 둘 다
  `null`(또는 빠짐)이면 지웁니다. 0은 그 방향을 막음(올리기 0이면 그 범위에 아무것도 내주지 않고, 받기 0이면 그 크루·채널의 첨부를
  자동으로도 `blob.fetch`로도 받지 않음), 1024개까지(`1005`). 올리기는 이 기기가 남의 요청(메시지 동기화·첨부 조각)에 답할 때
  요청한 크루·채널·사람에, 받기는 첨부 조각을 요청할 때 그 첨부의 크루·채널과 조각을 줄 사람에 걸립니다. 걸리는 제한(전체 포함)을
  모두 지켜야 보내므로 가장 낮은 값을 따릅니다. 범위별 버스트는 1초 분량이라 몇 MB짜리 파일도 처음부터 제한을 따릅니다. 크루에서
  나가면 그 크루와 채널의 제한은 지워집니다
- `transfer.rates() → TransferRates` (read, 공간 제한 토큰 불가) — `{up_bps, down_bps, uploads?: UploadRate[]}`. 지금 올리기·받기 속도
  (바이트/초, 최근 3초 평균). 모든 연결(Tor, 직접 연결, 파일 회로)의 요청·응답 바이트를 세고, 통화 미디어는 세지 않습니다.
  `UploadRate = {space, channel?, blob, user, bps}`: 이 기기가 내주는 첨부를 지금 받는 사람(5초 동안 조각이 없으면 빠짐, 빠른 것부터
  64명). 익명 채널의 첨부면 `user`가 `ANONYMOUS_USER`. 자동화 토큰에는 `uploads`를 비움
- `seed.coverage({space}) → {channel, online_seeders, online_archives}[]` — 인증된 멤버 기기에 `GetHeads`로 물어 답한(바쁨 포함)
  기기 수 + 내 설정이 그 채널을 제공하면 나. P2P 전용 채널은 함께 참여한 기기에게만 직접 연결로 묻고, 이 기기가 참여하지 않은
  P2P 전용 채널은 셀 수 없어 빠집니다. 기록이 없는 휘발성·카테고리·통화 채널도 빠집니다. `online_archives`는 그 채널을 보관하는
  지정된 보관 전용 노드 중 같은 방식으로 답한 수(시더와 따로 셈). 둘 다 0이면 경고 대상이고, 0이 된 채널마다
  `seed.coverage_warning` 알림을 한 번 보냅니다(0인 동안 다시 물어도 거듭 보내지 않음)
- 데이터 종류 이름: `text`, `image`, `video`, `file`, `emoji`, `history_bundle`

### 보관 전용 노드
복호화 키 없이 암호문만 보관·제공하는 노드(docs/protocol.md "보관 전용 노드"). 보관 노드 쪽 메서드는 `yangpad --archive`
(`YANGPA_ARCHIVE=1`)로 띄운 데몬에서만 씁니다.
- `archive.offer({expires_in_secs?, name?}) → {url, expires_at?}` (owner, 보관 노드 쪽) — 크루 관리자에게 따로 전할 1회용 지정 링크
  `https://dhtpdud.github.io/YANGPA/#archive/…`(`yangpa://archive/…`도 받음). 유효 시간 기본 24시간·최대 30일(0은 `1005`), `name`은 크루에 보일 제안 이름(64자 이하). `--archive`가
  아니면 `1003`, 쓰지 않은 링크가 64개면 `1010`
- `archive.hosted() → ArchiveHostedView[]` (보관 노드 쪽) — 받아 둔 크루마다 `{space, owner, members, channels, events, blobs,
  used_bytes, designated_by, designated_at, roster_issued_at}`. 내용은 읽을 수 없어 수치만
- `archive.drop({space}) → {}` (owner, 파괴적, 보관 노드 쪽) — 받아 둔 크루의 이벤트·첨부·기록을 모두 지움. 받아 둔 공간이 아니면
  `1004`. 지운 뒤에는 명단이 없어 그 크루 멤버의 연결을 받지 않으므로 다시 받아 두려면 새 지정 링크로 다시 지정해야 합니다.
  크루에는 알리지 않아 멤버 쪽 지정은 남고 멤버는 닿지 않는 보관 노드로 보고 물러나며 다시 시도하니, 관리자에게 해제를 부탁하세요
- `archive.designate({space, url, private_channels?, name?}) → ArchiveView` (admin, 파괴적) — 크루만, `manage_space`. 링크의 노드에
  연결해 첫 명단을 넘기고, 받아들이면 `ArchiveGrant` 상태 이벤트를 발행. 공개 채널은 늘 보관하고 비공개 채널은 `private_channels`
  (내가 볼 수 있는 것, 휘발성 아님)만. 링크가 잘못되거나 만료·이미 씀, 멤버·밴된 사용자의 노드는 `1005`, 크루마다 8개 초과는
  `1010`, 노드에 닿지 못하면 `1009`, 노드가 거절하면 `1003`
- `archive.revoke({space, device}) → {}` (admin, 파괴적) — `manage_space`. `ArchiveRevoke`를 발행하고, 권한자 기기들이 보관 노드에
  해제를 알려 데이터를 지우게 함(닿을 때까지 다시 보냄). 지정되지 않은 기기면 `1004`
- `archive.list({space}) → ArchiveView[]` — 지정 순서대로 `{device, user, name, private_channels, granted_by, granted_at, online}`.
  `online`은 지금 인증된 연결이 있는지

### 네트워크·Tor
앱 사용자 기기는 늘 Tor로 돕니다(켜고 끄는 메서드가 없음, protocol.md "Tor 전용 연결"). 직접 연결은 이 기기가 참여한 P2P 전용
채널(`channel.p2p_join`)과 신뢰 스위치(`trust.*`)로만, 따로 띄운 P2P 쪽 스웜으로 하고, 아래 상태 메서드는 주 스웜만 보여 줍니다. 저전력, 상시 노드
(부트스트랩 주소·공개 노드) 설정, IP 숨기기와 예전 통화용 STUN·TURN 설정 메서드는 없어졌고(decisions.md, 2026-10-22), 예전에 저장한
그 설정은 데몬이 시작할 때 지웁니다. 2026-10-23에 더한 TURN 설정(`net.turn`·`net.set_turn`·`net.check_turn`)도 2026-11-07에
없어졌고, 저장된 `turn.servers`·`turn.kinds`는 같은 방식으로 지웁니다.
- `net.status() → NetStatus` — `{peer_id, mode, listen_addrs, relay_addrs, nat, connections, bootstrap_progress?, external_addrs, onion_addr?, peers, connecting?}`. `bootstrap_progress`는 Tor 부트스트랩 진행률(0~100)로 100은 회로를 만들 수 있을 때만입니다(준비 전에는 99까지). `connecting`은 시작할 때 다시 거는 노드 연결(인증까지)이 아직 끝나지 않은 친구·나(내 다른 기기)의 사용자 ID 목록, 비면 빠짐(하나 끝날 때마다 `net.status` 알림. 앱은 그 친구를 '연결 중'으로 보임). `offline: true`는 오프라인 모드(망 없이 뜸, `presence.set`)
  - `mode`는 사용자 기기면 늘 `"tor"`입니다. `"normal"`은 직접 연결로 띄운 데몬(`yangpad --direct`/`YANGPA_DIRECT`, 시더·
    시험용, `--relay-server`도 이것)뿐입니다. Tor를 준비할 수 없으면 데몬이 시작하지 않습니다(직접 연결로 넘어가지 않음).
  - 저장된 값이 없는 시더·피어 설정은 Tor 기본값이고, 링크 미리보기는 Arti로만 조회합니다(Arti를 쓸 수 없으면 만들지 않음).
  - 주소 필드(`listen_addrs`·`relay_addrs`·`external_addrs`)는 CLI·진단용입니다. 데스크톱 앱은 IP 주소를 화면에 보이지
    않습니다(onion 주소만).
- `net.stun_servers() → StunServers` (read) — `{servers: string[], default: bool}`. 직접 연결로 나가는 경로(P2P 전용 채널·신뢰
  스위치의 직접 스웜, 직접 연결로 띄운 데몬의 SFU 호스트)가 공유기 밖 주소를 알아낼 때 묻는 STUN 서버(`host:port`, 앞쪽부터).
  Tor 연결에는 쓰지 않습니다. 저장한 적이 없으면 데몬 기본 목록(`stun.l.google.com:19302`, `stun1.l.google.com:19302`,
  `stun.cloudflare.com:3478`)과 `default: true`. 빈 목록이면 STUN을 쓰지 않습니다. 쓰기로 한 크루의 STUN 서버는 이 목록
  뒤에 붙습니다(`crew.use_servers`).
- `net.set_stun_servers({servers: string[] | null}) → {}` (settings, 사람 토큰만) — `null`이면 저장한 목록을 지워 기본 목록으로,
  `[]`이면 STUN을 쓰지 않습니다. 항목은 `host:port`(앞의 `stun:`은 떼고, 포트가 없으면 3478, `[IPv6]:port`도 됨)로 맞춰
  소문자·중복 없이 저장합니다. 8개까지, 호스트 253자까지. `turn:`·`turns:`(TURN), 사용자 이름·비밀번호(`@`), 경로·쿼리가 든
  항목은 `1005`로 거절합니다. 고른 서버가 이 기기의 공인 IP를 보게 되므로 자동화 토큰은 `1002`.
- `net.file_circuits() → FileCircuits` (read) — `{enabled: bool, count: 0~4}`. Tor 파일 회로(큰 첨부를 Tor로 주고받을 때 상대마다
  파일 전용 회로를 따로 여는 것, [protocol.md](protocol.md) "Tor 전용 연결")를 상대 하나에 몇 개 여는지(`count`, 저장한 적이 없으면
  0)와 쓰는지(`enabled = count > 0`). 예전 빌드가 저장한 켜기·끄기는 4/0으로 읽습니다.
- `net.set_file_circuits({count?: 0~4, enabled?: bool}) → {}` (settings, 사람 토큰만) — 1 이상이면 이 기기가 받을 때 상대마다 그 수만큼
  파일 회로를 열고(여는 수는 받는 쪽 설정), 상대의 파일 링크도 받습니다. 양쪽이 모두 1 이상이어야 쓰입니다. 0이면 이어 둔 파일
  스웜을 멈추고 들어온 파일 링크를 지우고, 줄이면 넘는 번호의 회로를 닫습니다. 4를 넘으면 `1005`. `count`가 없으면 예전 호출처럼
  `enabled`가 참이면 4, 아니면 0. 회로가 많을수록 이 기기의 onion 서비스가 회로를 더 자주 만들어 가드 찾기 공격에 더 드러나므로
  자동화 토큰은 `1002`.
- `net.channel_topics() → ChannelTopics` (read) — `{enabled: bool}`. 채널 묶음·알림 토픽([protocol.md](protocol.md) "실시간 전파")을
  쓰는지. 저장한 적이 없으면 거짓.
- `net.set_channel_topics({enabled: bool}) → {}` (settings, 사람 토큰만) — 켜면 gossip을 쓰는 크루(활성 기기 8대 초과)에서 알림
  토픽과 최근 7일 안에 연 채널의 묶음 토픽만 구독하고, 다른 모두 보는 채널의 새 글은 그 채널을 열 때 받아 옵니다. 모든 공간의
  구독을 바로 맞춥니다. 켠 뒤 연 채널만 기록합니다.
- `net.bridges() → BridgesView` (read, 사람 토큰만) — `{enabled, kind: "snowflake" | "obfs4" | "custom", custom: string[], qr?,
  auto?, available}`. Tor 브리지([protocol.md](protocol.md) "Tor 브리지"). `kind`는 꺼져 있어도 고른 것, `custom`은 직접 넣은
  줄, `qr`은 그 줄들(줄바꿈으로 이음)의 QR 칸(`device.link.create`의 `qr`과 같은 모양, QR에 다 들어가지 않으면 빠짐), `auto`는 Tor에 들어가지
  못해 데몬이 Snowflake를 스스로 켰음, `available`은 브리지 전송 프로그램(lyrebird)이 있음. 직접 넣은 줄은 공개되지 않은 브리지
  주소라 자동화 토큰은 `1002`.
- `net.set_bridges({enabled, kind, custom}) → {}` (settings, 사람 토큰만) — 저장하고 떠 있는 Tor에 바로 씁니다(브리지로 다시
  연결). `custom`은 앞뒤 공백·빈 줄을 빼고 32줄(줄마다 2KiB)까지이고, 고른 종류가 `custom`이 아니어도 저장합니다. 읽을 수 없는
  줄·lyrebird가 맡지 않는 전송(conjure 등)·33줄 이상·`custom`인데 줄 없이 켜기·lyrebird 없이 켜기는 `1005`. 한 번 저장하면 자동
  Snowflake는 하지 않습니다(끄기를 고른 것도 존중).
- `net.peers() → {peer_id, user?, rtt_ms?, relayed}[]` — 주 스웜에 연결된 피어

### 음성·화면공유
- `voice.join({space, channel}) → {call, participants: VoiceParticipant[], media_key: base64, generation, topology: "mesh" | "sfu", sfu_host?, sfu_host_user?, ice_servers: IceServer[]}`
  - 통화 채널(`kind: "voice"`)에 `view_channel`·`connect_voice` 권한이 있어야 합니다. 다른 통화에 있으면 먼저 나옵니다.
  - 돌아올 때 `participants`에는 이미 참가 중이던 기기가 모두 있습니다. `media_key`는 키 관리 기기가 보낸 지금 세대
    키이고, 아직 받지 못했으면 `generation: 0`과 쓰지 않을 임시 키입니다(받는 대로 `voice.participants`로 옴).
  - `ice_servers`는 내 STUN 서버 뒤에 그 크루의 쓸 수 있는 STUN 서버를 붙여 `stun:` URL들을 하나로 묶은 서버 하나입니다(둘 다
    비면 빈 배열). `IceServer = {urls: string[]}`. TURN은 없고 앱은 ICE 정책 `all`로 잇습니다.
  - 자동화 토큰에는 프레임 키를 주지 않습니다(`generation: 0`, 빈 `media_key`). 통화 내용은 AI에게 열지 않습니다.
  - P2P 전용 채널은 이 기기가 `channel.p2p_join`으로 참여해야 하고(아니면 `1003`), 참여했으면 직접 후보로 통화합니다(함께
    참가한 사람에게 IP가 보임). 그 밖의 채널은 Tor로 도는 기기면 `topology: "tor"`(Tor 통화: 소리는 `voice.media`로, 화면공유는
    비상용, protocol.md "Tor 통화")이고, 직접 연결로 띄운 데몬은 WebRTC로 통화합니다. Tor 통화에서도 서로 신뢰한(신뢰 스위치)
    참가자는 `VoiceParticipant.direct`가 참이고 앱이 그 상대와만 WebRTC로 잇습니다. 그래서 켜 둔 신뢰가 있으면 `ice_servers`를
    주고, 없거나 익명 채널이면 비웁니다. 참가하는 동안
    P2P 전용 채널 참여를 그만두거나 채널이 Tor로 돌아가면 통화에서 나옵니다.
  - `topology`(`mesh`·`sfu`·`tor`)·`sfu_host`는 키 관리 기기의 결정입니다(protocol.md "SFU"). `tor`는 이 기기가 정하며 다른 기기의 결정과 상관없습니다. `sfu`인데 `sfu_host`가 없으면 호스트를 넘겨받는
    중이니 다음 `voice.participants`를 기다립니다. `sfu_host`는 참가자가 아닌 기기(호스트만 맡는 노드, 그 채널에
    `manage_voice`가 있는 멤버의 것)일 수 있고, `sfu_host_user`는 그 기기의 사용자입니다(앱이 누가 중계하는지 보여 줌).
- `voice.leave({space, channel}) → {}`
- `voice.signal({space, channel, call, to: DeviceId, payload: SignalPayload}) → {}` — 앱은 메시면 `offer`·`answer`·`ice`·
  `watch_screen`을 지금 참가자에게, SFU면 `sfu_offer`·`sfu_answer`·`sfu_ice`·`sfu_select`·`sfu_close`를 지금
  `sfu_host`에게만 보냅니다(`join`·`leave`·`state`·`media_key`·`sfu_plan`·`sfu_host`는 데몬끼리만, 보내면 `1005`).
  `call`은 이 기기의 통화 ID여야 합니다. SDP 64KiB·ICE 후보 4KiB·트랙 표 8개까지. SFU 신호를 지금 호스트가 아닌 기기에
  보내면 `1010`. 내가 호스트면 이 데몬의 SFU가 바로 받아, 맡은 통화방이 아직 없으면 `1004`, 통화방에 들어올 수 없는
  기기면 `1003`, 가득 찼으면 `1010`, 지난 세션 번호·읽을 수 없는 SDP·잘못된 트랙 표면 `1005`입니다. 다른 노드가
  호스트면 거절이 `1009`로 옵니다. 거절되면 앱은 잠시 뒤 더 큰 `session`으로 다시 제안합니다. `sfu_answer`를 보내지
  못했을 때도 바로 새 세션으로 다시 붙습니다(호스트는 응답 없는 재협상을 10초 뒤 닫음). `sfu_select`의 `device`가 그
  통화방의 다른 참가자가 아니면 `1003`입니다.
- `voice.media({space, channel, call, data, to?: DeviceId[]}) → {}` — Tor 통화(`topology: "tor"`)의 소리·화면 패킷 하나. `data`는 앱이
  미디어 키로 봉인한 바이트(base64, 1~15KiB)이고, 데몬은 내용을 보지 않고 지금 참가자(`to`가 있으면 그중 그 기기들)에게 Tor 연결로
  보냅니다(기다리지 않음, 밀리면 버림).
  Tor 통화가 아니면 `1010`, 참가 중이 아니거나 `call`이 다르거나 크기가 맞지 않으면 `1005`. 성공한 호출은 감사 로그에 남지 않습니다.
  앱은 WebRTC로 이어진(서로 신뢰한) 상대를 뺀 참가자를 `to`에 넣습니다.
  Tor 통화에서는 `voice.signal`의 `sfu_*`·`watch_screen`과, 이 기기가 신뢰하지 않는 참가자에게 가는 `offer`·`answer`·`ice`를
  `1010`으로 거절합니다(신뢰한 참가자에게는 메시처럼 보냄).
- `voice.state({space, channel, muted, deafened, screen}) → {}` — `screen: true`는 `stream` 권한, `speak` 권한이
  없으면 늘 음소거로 알립니다.
- `voice.watch({space, channel, device, watching}) → {}` — 화면공유 중인 참가자만 볼 수 있습니다.
- `VoiceParticipant = {user, device, anon?, anon_name?, mine?, muted, deafened, screen, viewers?: DeviceId[], direct?}` — `viewers`는 이 기기가 아는
  시청자(자기 화면이면 보는 기기 전부, 남의 화면이면 보고 있을 때 자기 기기). `mine`은 이 기기의 줄. `direct`는 Tor 통화에서
  이 기기와 서로 신뢰해 WebRTC로 바로 잇는 참가자(익명 채널은 늘 거짓, 거짓이면 생략). 신뢰가 바뀌면 `voice.participants`가
  다시 옵니다. 익명 채널이면 `user`가
  `ANONYMOUS_USER`이고 `anon_name`에 그 채널의 가명(`MessageView.anon_name`과 같은 이름), `anon`에 번호('익명 1', '익명 2' …, 이 기기에
  들어온 순서, 통화가 비면 처음부터, 옛 앱용)가 있습니다. `device`는
  연결에 써야 해서 그대로입니다. 익명이 꺼지면 데몬이 스스로 통화에서 나오고, 내 기기가 빠진 `voice.participants`로 알립니다(다른 이유로 나올 때와 같음).
- `SignalPayload`(`type`으로 구분): `offer {sdp}`, `answer {sdp}`, `ice {candidate, sdp_mid?, sdp_mline_index?}`,
  `watch_screen {watching}`, `leave`(참가자가 나가거나 노드가 끊김). 앱이 받는 `voice.signal`의 `call`은 자기 통화 ID입니다.
- SFU `SignalPayload`(앱 ↔ 지금 호스트, protocol.md "SFU"):
  - `sfu_offer {session: u32, sdp, tracks: SfuTrack[]}` — 앱 → 호스트는 새 세션(세션마다 한 번, 올릴 트랙을 다 담음,
    `tracks`는 `{mid, source}` 4개까지·출처마다 하나). 호스트 → 앱은 같은 세션의 재협상이고 `tracks`에 받을 트랙 전부와
    출처 기기(`{mid, device, source}`)가 있습니다. 앱은 응답(`sfu_answer`)만 하고 다시 제안하지 않습니다.
  - `sfu_answer {session, sdp, tracks?}`, `sfu_ice {session, candidate, sdp_mid?, sdp_mline_index?}`(앱 → 호스트만),
    `sfu_close {session}`(앱 → 호스트: 나감, 호스트 → 앱: 세션을 닫음 → 새 세션으로 다시 붙음).
  - `sfu_select {device, source, layer}` — 받을 트랙과 simulcast 층. `source`: `audio`·`camera`·`screen`,
    `layer`: `auto`·`high`·`medium`·`low`·`off`. 화면은 고르기 전 `off`(‘보기’ 전에는 받지 않음), 나머지는 `auto`.
  - 화면공유 simulcast는 rid `q`·`h`·`f`(낮은 층부터)로 올립니다. 내가 호스트면 내 데몬의 SFU 신호는 `from`이 내 기기로 옵니다.

### 토큰·감사·사람 확인
- `token.create({name, scopes, spaces?, automation?}) → {token, id}` (owner) — `token` 원문은 이때 한 번만
  나오고 데몬에는 해시만 남습니다. 이름 1~64자, 범위 하나 이상, `automation` 토큰에는 `owner` 불가(`1005`), 허용 공간은 64개
  이하이고 아는 공간이어야 함(`1004`), 쓸 수 있는 토큰은 256개까지(`1010`)
- `token.list() → TokenView[]` (settings) — `{id, name, scopes, spaces, automation, created_at, last_used_at?}`, 폐기한 토큰과
  원문은 없음
- `token.revoke({id}) → {}` (settings) — 새 연결의 인증뿐 아니라 이미 붙은 연결의 다음 호출도 `1001`이고, 그 연결로 가던
  알림도 멈춥니다. 그 토큰이 낸 확인 요청도 지웁니다. 소유자 토큰(`owner`)은 `1005`, 없거나 이미 폐기했으면 `1004`
- `audit.list({limit?, before?}) → AuditEntry[]` (settings) — 최신순, `limit` 기본 100(1~1000), `before`는 이 `id`보다 오래된
  것만. `AuditEntry = {id, at, method, token_name, ok, code?, automation}`: 인자·결과·메시지 내용은 남기지 않습니다. 중요한
  줄(실패, 사람이 승인한 실행, 파괴적·사람 전용 메서드, `admin`·`owner` 범위, `token.*`·`confirm.*`)은 최근 5만 줄, 나머지는
  최근 1만 줄을 따로 보관해 평범한 호출을 잔뜩 해도 중요한 기록이 밀려나지 않습니다(인증하지 않은 `auth.hello` 실패는 평범한 줄)
- `confirm.list() → PendingConfirmation[]` — AI가 요청한 파괴적 작업(만료된 것은 빠짐). 자동화 토큰은 `1002`, 공간 제한
  토큰은 그 공간의 요청만 봅니다. `PendingConfirmation = {id, method, params, token_name, space?, requested_at,
  expires_at, summary}`(`summary`는 사람이 읽는 한국어 요약, 메시지 내용 없음)
- `confirm.approve({id}) → {result}` (admin, 사람 토큰만) — 요청을 꺼내(한 번만) 요청한 토큰의 범위·공간 제한으로 원래 호출을
  실행하고 그 결과를 `result`로 돌려줍니다. 승인하는 토큰도 원래 메서드의 범위와 그 공간 권한이 있어야 합니다. 없거나 이미
  처리했거나 만료(기본 1시간)됐으면 `1004`, 요청한 토큰이 폐기됐으면 `1002`
- `confirm.deny({id}) → {}` (사람 토큰만) — 실행하지 않고 지웁니다
- 대기열은 100개까지(넘으면 새 요청이 `1010`). 확인을 기다리는 동안 `confirm.pending` 알림이 갑니다. 휘발성 채널을 대상으로
  한 요청은 디스크에 쓰지 않고 메모리에만 두므로 데몬을 다시 시작하면 사라집니다.

## 이벤트 구독

`events.subscribe({topics?: string[]}) → {subscription}` 후 서버가 알림을 보냅니다. 알림은 화면 프레임(약 16ms) 단위로 묶어
`{"jsonrpc":"2.0","method":"events","params":{"batch":[Notification,...]}}`로 옵니다.

| `type` | 내용 |
| --- | --- |
| `message.new` | `{space, channel, message: MessageView, notify: {title, body?, sender?: {user, name, avatar?}}}` — 제목은 작성자 이름(크루면 `이름 · 크루 #채널`), 본문은 앞 100자(멘션은 `@이름`, 커스텀 이모지는 `:이름:`, 글 없이 첨부만이면 "첨부 파일"). `sender`는 OS 알림 프사용(표시 이름과 프로필 이미지 블롭, 이미지는 이 공간으로 `blob.read`). 휘발성은 제목 "새 메시지"에 body·sender 없음. 자동화 토큰 연결은 API 읽기를 허용하기 전까지 휘발성 내용(본문·반응)이 빠진 알림만. 스냅숏으로 받은 메시지는 알리지 않음. 내가 차단한 사용자의 메시지와 신고하며 숨긴 메시지는 알리지 않음(채널을 다시 읽으면 차단한 사람 것은 `author_blocked: true`로 들어 있음) |
| `message.updated` | `{space, channel, message: MessageView}` (수정·반응·삭제, 밴의 최근 메시지 삭제 포함). 숨긴 메시지는 알리지 않음 |
| `message.expired` | `{space, channel, ids}` — 휘발성: 수명 만료, 퇴장(내 기기), 수명 '퇴장 시'인 채널에서 작성자 퇴장 |
| `channel.typing` | `{space, channel, user, anon_name?}` — 다른 사람이 그 채널에서 글을 쓰는 중(`channel.typing`을 받음). 보낸 기기가 그 채널에 글을 쓸 수 있는 멤버이고 길이 맞을 때만, 차단한 사람·내 다른 기기는 빼고, 기기·채널마다 1초에 한 번까지. 끝났다는 알림은 없어 앱이 몇 초 뒤(데스크톱 6초) 또는 그 사람의 `message.new`에 지움. 익명 채널이면 `user`는 `ANONYMOUS_USER`, `anon_name`에 가명 |
| `space.updated` | `{space}` (상태 변경: 채널·역할·멤버, 키 갱신으로 `pending_commit`·`key_rotation_pending`이 바뀔 때, 확정 기기 부정 증거를 남길 때) |
| `friend.request` | `{request_id, user, profile}` — 임시 연락처가 친구 추가를 보냄 |
| `friend.accepted` | `{user, space}` — 연락처 링크로 임시 연락처가 됨(1:1 공간이 생김) |
| `friend.updated` | `{user}` — 연락처 종류가 바뀜(임시 연락처가 만료되거나 친구가 됨, 만료된 연락처가 새 링크로 다시 임시 연락처가 됨). `friend.list`를 다시 읽으세요 |
| `presence` | `{user, online, away?, last_seen?}` — 화면에 보일 접속 상태가 바뀜(`MemberView`의 같은 이름 필드와 같은 뜻). 숨기 중인 기기가 나타나거나 사라져도 오지 않음. 내 표시 상태가 바뀌면 내 사용자로도 옴(`last_seen` 없음) |
| `presence.self` | PresenceView — 이 기기의 표시 상태가 바뀜(`presence.set`, 기간 끝남, 입력 없음) |
| `flair` | FlairView — 사용자(나 자신도)의 후원자 표시가 바뀜. 공간에 묶이지 않음 |
| `profile.updated` | `{user}` — 받은 친구·멤버(또는 내 다른 기기)의 서명 프로필이 더 새 판으로 바뀜. 이 기기의 `profile.set` 뒤에도 내 사용자로 옴. `friend.list`·`space.get`을 다시 읽으세요. 공간에 묶이지 않아 공간 제한 토큰에는 가지 않음 |
| `badges.updated` | `{user}` — 그 사람(나 자신도)의 뱃지 카드가 바뀜. `badge.get`을 다시 읽으세요. 공간에 묶이지 않음 |
| `voice.signal` | `{space, channel, call, from: DeviceId, payload}` — `write` 범위 사람 토큰에만(ICE 후보·SDP에 상대 IP가 들어 있음) |
| `voice.media` | `{space, channel, call, from: DeviceId, data}` — Tor 통화의 봉인한 소리 패킷(base64). `write` 범위 사람 토큰에만 |
| `voice.participants` | `{space, channel, participants, generation, media_key, topology, sfu_host?, sfu_host_user?}` — 참가자·상태·시청자·키 세대·토폴로지(SFU 호스트와 그 사용자)가 바뀔 때마다. 참가 중이 아니면 `generation: 0`과 빈 키. `write` 범위 사람 토큰이 아니면(자동화 토큰 포함) 늘 `generation: 0`과 빈 키 |
| `blob.progress` | `{space, blob, have, total}` |
| `net.status` | NetStatus |
| `net.bridges` | BridgesView — 브리지 설정이 바뀜(`net.set_bridges`, 또는 Tor에 들어가지 못해 데몬이 Snowflake를 켬: `auto`). 자동화 토큰에는 가지 않음 |
| `transfer.rates` | TransferRates — 1초마다 재고 값이 바뀔 때만(모두 0이 되면 한 번 보내고 멈춤). 공간 제한 토큰에는 가지 않고, 자동화 토큰에는 `uploads`가 빠짐 |
| `confirm.pending` | PendingConfirmation |
| `seed.coverage_warning` | `{space, channel}` |
| `report.new` | `{space, report_id}` — 운영진 기기에서 새 신고를 받았을 때(내용은 `report.list`로) |
| `device.link_request` | `{name, code, contested}` — 띄운 기기 연결 링크로 새 기기가 승인을 청함(`device.link.decide`). 같은 링크를 다른 기기가 또 쓰면 `contested: true`로 다시 옴. 공간에 묶이지 않아 공간 제한 토큰에는 가지 않음 |
| `device.link_result` | `{status}` — 이 새 기기의 연결 요청이 끝남. `status`: `approved`(신원을 설치함, `identity.get`의 `first_syncing`이 켜짐), `rejected`, `expired`, `failed`(승인됐지만 받은 묶음이 링크와 맞지 않거나 저장하지 못함) |
| `device.synced` | `{}` — 연결된 기기의 첫 동기화가 끝남(`identity.get`의 `first_syncing`이 꺼짐) |
| `supporter.updated` | `{error?}` — 후원 확인 상태가 바뀌었거나 로그인이 끝났음(`supporter.status`로 다시 읽음). 로그인 실패면 `error`: `denied`(거절)·`expired`(시간 초과)·`creator`(크리에이터 본인 계정이라 버림)·`network`(확인 실패)·`failed`. 공간에 묶이지 않음 |
| `update.progress` | `{version, done, total, ready?, error?}` — `update.download` 진행(바이트, 0.5초마다). `ready`면 서명·SHA-256을 확인하고 다 받음. 실패면 `error`: `unsigned`(서명 없는 릴리스이거나 목록에 없음)·`network`·`hash`(SHA-256 불일치)·`failed`(디스크). 공간에 묶이지 않음 |
| `device.wiped` | `{}` — 이 기기가 다른 기기에서 폐기됐거나 `identity.delete`로 로컬 데이터를 모두 지웠음(신원 없음, 네트워크 설정만 남음) |
| `checkpoint.alert` | `{space, kind}` — 상태 체크포인트 경보(protocol.md "상태 체크포인트"). `kind`: `conflict`(같은 순번에 서명이 맞는 다른 체크포인트가 보임: 정한 규칙으로 한쪽을 따르지만 소유자 기기가 갈라졌거나 탈취됐을 수 있음), `equivocation`(같은 기기가 같은 순번에 둘을 씀), `fallback`(체크포인트로 들어온 상태가 다른 출처와 다르거나 확인되지 않아 버리고 처음부터 따라잡는 중). 겨룬 체크포인트 하나에 한 번. 공간 제한 토큰에는 그 공간 것만 |
