# 데몬 내부 설계 (`yangpad`)

[architecture.md](architecture.md)의 데몬 모듈 목록을 실제 흐름으로 풀어 쓴 문서입니다. 코드는 `crates/yangpa-daemon`.

## 시작과 종료

```rust
pub struct Node {
    cfg: NodeConfig,                        // 데이터 경로, 키 보관 방식, 네트워크, 동기화 간격, 소유자 토큰 파일 여부
    store: Arc<Store>,
    net: NetHandle,
    me: RwLock<Option<Arc<Me>>>,            // 신원이 없으면 None (identity.create 전)
    spaces: RwLock<HashMap<SpaceId, SpaceRuntime>>,
    peers: PeerDirectory,                   // PeerId ↔ (사용자, 기기), 연결·인증 상태
    notify: Notifier,                       // 로컬 API 구독자(EventHub)와 프로세스 안 구독자에게 알림
    tasks: TaskSet,                         // 노드가 띄운 모든 작업(오래 도는 작업, spawn_detached, 들어온 요청 처리). 종료 때 멈추고 닫음
    ingest_gate: Arc<tokio::sync::RwLock<()>>, // 적용 관문: 공간 하나는 읽기, 여러 공간·종료는 쓰기. 종료가 끝나면 돌려주지 않음
    ingest_spaces: Mutex<HashMap<SpaceId, Arc<tokio::sync::Mutex<()>>>>, // 공간마다 작성·적용을 한 줄로(다른 공간은 동시에)
    devkeys: DeviceKeyTable,                // 키 약속 → 기기 공개키(DB device_keys 앞의 LRU)
    identity_lock: Mutex<()>,               // 신원 설치·프로필 바꾸기를 한 줄로
    sync: SyncState, keygroup: KeyGroupState,   // 키 그룹: 묶음 창·바퀴, 확정 기기 표 캐시, 보내는 쪽 규칙, 중계 기억
    crews, volatile, blobs, policy, voice, admin, notes, moderation, tor, locator, diag, archive, p2p,  // 다른 작업 영역의 상태
}
struct Me { identity: Identity, device: DeviceKeys, devices: SignedDeviceList, profile: SignedProfile }
struct SpaceRuntime { state: Option<Arc<SpaceState>>, genesis, genesis_lamport, member_since, dormant,
                      core: Arc<Mutex<SpaceCore>>,    // SpaceCore = 해소기, 키 그룹 멤버 상태(keys), 대기·보류 목록,
                                                      // persisted(마지막으로 저장한 판: 바뀐 행만 쓰는 기준)
                      scoped_seen: Arc<Mutex<ScopedSeen>> }  // 따라잡기가 읽는 부분 전달 기억(잎 잠금)
```

- 적용 잠금 순서(교착 방지, `ingest::Locked` 주석): 관문(`ingest_gate`) → 공간 잠금(`ingest_spaces`) → `SpaceCore` →
  `Node::spaces` 표. 공간 하나만 다루는 작성·적용·따라잡기·확정 기기 일과 공간 하나를 만들거나 지우는 일(크루 만들기·탈퇴,
  친구 공간 만들기·정리, 내 기기가 알려 준 공간 받아들이기)은 `ingest::lock_space`(관문 읽기 + 그 공간 잠금 하나, 만들 때는 새 ID로 먼저
  잠금: `spaces::create_space`), 여러 공간을 다루는 일(폐기된 기기 지우기, 해소되지 않은 받아들인 공간 정리)만 `ingest::lock`
  (관문 쓰기, 모든 공간을 막음)입니다. 관문은 쓰기 대기가 있으면 새 읽기를 세우므로, 쓰기를 드물게 써야 큰 크루의 긴 적용
  동안 다른 공간이 줄을 서지 않습니다. 잠금을 쥔 채 다른 적용 잠금을 기다리지 않고, 적용 함수는 잠금이 그 공간을 덮는지
  확인합니다(`Locked::check`). 공간 잠금 표(`ingest_spaces`)는 약한 참조라 쥐거나 기다리는 쪽이 없는 잠금은 남지 않습니다.
  그래서 큰 크루의 따라잡기가 다른 공간의 메시지 보내기·받기를 막지 않습니다. 적용은 `SpaceCore`(parking_lot)를 적용 내내
  쥐므로, 비동기 작업은 공간 잠금 없이 `SpaceCore`를 잡지 않습니다(잡으면 긴 적용 동안 작업 스레드가 묶여 데몬 전체가 멈춤).
  따라잡기가 동기 지문 비교에서 읽는 부분 전달 기억(대기 중·거부한 ID, 끝난 따라잡기 바퀴)은 그래서 `SpaceRuntime::scoped_seen`
  잎 잠금에 두고 적용 경로가 대기열을 바꿀 때 함께 고칩니다(`scoped::known_ids`·`ingest::note_caught_up`). 잎 잠금(전달자 표,
  피어 표, `scoped_seen`)은 어느 단계에서나 잠깐 쥐고, 쥔 채 다른 잠금을 잡지 않습니다.

- `Node::start(cfg)`(또는 RPC 서버와 같은 `EventHub`를 쓰는 `start_with_hub`):
  데이터 경로(0700) → DB 키(아래) → SQLCipher 저장소 → 네트워크 설정(`tor::net_config`: 예전 빌드가 남긴 모드 설정
  `net.tor`·`net.tor_open`·`net.friend_onion`·`relay.sticky`와 없어진 망 설정 `net.hide_ip`·`net.low_power`·`net.bootstrap`·
  `net.public_nodes`·`net.ice_servers`, 2026-11-07에 뺀 TURN의 `turn.servers`·`turn.kinds`를 지우고(`tor::LEGACY_SETTINGS`), Tor로 도는 기기는 직접 연결 키 `NET_KEY`도 지움. 기본은
  Tor 스웜·`NET_KEY_TOR`이고 이 빌드가 Tor를 지원하지 않거나 Arti 상태 디렉터리를 만들 수 없으면 시작 실패. `NodeConfig.net.direct`
  (`--direct`)일 때만 직접 연결 스웜·`NET_KEY`. 부트스트랩은 시작 인자(`NodeConfig.net`)만 따름) → 네트워크 →
  신원 읽기 → 소유자 토큰 파일(`owner.token`, 0600, 없으면 만듦) → 저장된 공간 다시 세우기(`spaces::load_all`: 공간
  스트림 이벤트와 대기 이벤트를 다시 적용(이벤트 서명·본문 서명을 다시 확인하고 해소. 저장된 상태 행은 바뀐 행만 쓰는 기준일
  뿐이라 3만 명 크루면 서명 확인만 10초 넘게 걸림, performance.md "상태 저장·색인". 바탕 체크포인트가 있으면 저장한 스냅숏으로
  해소기를 세우고 경계 F 이상 이벤트만 다시 넣음(`checkpoint::restore`, performance.md "상태 체크포인트")), 알림 없음, 키 그룹
  멤버 상태는 잠긴 키 저장소에서 필요할 때 읽음) → 다른 영역의
  `start` 고리(`p2p::start`가 마지막: 참여한 P2P 전용 채널이나 켠 신뢰 스위치, 내 다른 기기가 있으면 직접 스웜을 띄움) → 네트워크 이벤트 처리 → 주기 동기화 →
  끊긴 기기 다시 걸기 → 최근 노드에 다시 연결(`peer_cache`) → 임시 연락처 만료 검사(`friends::spawn_contact_expiry`, 5초마다).
- DB 키(`KeyStorage`): `File`은 `db.key`(0600). `OsKeyring`은 키 저장소만. `Auto`는 `db.key`가 있으면 그것, 없으면 키 저장소에서
  읽고, 키 저장소에 항목이 없을 때만(DB가 아직 없을 때) 새로 만듭니다. **DB가 이미 있는데 키를 읽을 수 없으면**(키 저장소가 잠김·
  시간 초과·항목 없음) 새 키를 만들지 않고 시작을 멈춥니다(새 키로 바꾸면 기존 DB를 영영 못 열기 때문). DB가 없고 키 저장소를
  쓸 수 없을 때만 `db.key`로 내려갑니다.
- `Node::shutdown()`: 여러 곳에서 동시에 불러도 한 번만 하고 모두 끝날 때까지 기다립니다(종료 작업은 따로 띄워, 부른 작업이
  취소돼도 끊기지 않음). 작업 중지(`TaskSet`이 닫혀 이후 `spawn`은 무시) → P2P 쪽 스웜(직접·출구) 내리기(`p2p::shutdown`) → 네트워크 종료
  (수신 소켓이 닫힐 때까지 최대 5초) →
  진행 중 적용이 끝나기를 기다려 적용 잠금을 쥔 채 휘발성 상태 전부 지움(`volatile::wipe_all`) → WAL 체크포인트. 잠금은 돌려주지 않으므로 종료한 노드가 같은 DB에 늦게 쓰지 않습니다(그래서 종료한 `Node`로 작성·적용하는 호출은
  돌아오지 않습니다. 종료 뒤에는 그 `Node`를 버리세요).
- `main.rs`(`yangpad`): 윈도에서는 맨 먼저 전원 조절(EcoQoS, 효율 모드)에서 빠집니다(`SetProcessInformation(ProcessPowerThrottling)`,
  실행 속도 조절 끔. 창 없는 데몬이 앱 창을 내리거나 가린 동안 느려지지 않게, 실패하면 그대로 돎. 다른 OS는 할 일 없음) →
  인자(`--data-dir`/`YANGPA_DATA_DIR`, `--endpoint`/`YANGPA_ENDPOINT`, `--listen`, 도움말에 숨긴
  `--bootstrap`(부트스트랩·릴레이 노드 주소, 시험·운영용. 사용자 설정의 상시 노드는 없어짐),
  `--relay-server`(`--direct`를 켬), `--direct`/`YANGPA_DIRECT`(Tor를 거치지 않는 직접 연결 스웜으로 시작. 공인 주소로 받는
  상시 시더·중계 서버·시험용이고, 연결되는 상대에게 IP가 보임. 값은 `true`/`false`. 끄면(기본) Tor로만 돌고 준비할 수 없으면
  시작 실패), 숨긴 예전 옵션 `--default-mode tor|normal`/`YANGPA_DEFAULT_MODE`(`normal`이면 `--direct`와 같음. 예전 서버 패키지의
  환경 변수를 계속 읽으려고 둠)와 `--tor`(`YANGPA_DEFAULT_MODE=normal`을 덮어 Tor로 띄움. `--direct`·`--relay-server`와 함께
  쓰면 시작 실패), 도움말에 숨긴 `--p2p-listen <MULTIADDR>`(여러 번, P2P 쪽 직접 스웜의 수신 주소. 주면 STUN·UPnP로 바깥에 묻지 않음.
  시험용), `--no-mdns`, `--key-storage`, `--sync-interval`, `--device-name`,
  `--dormant-after`/`YANGPA_DORMANT_AFTER`(예: `14d`, `0`이면 휴면 끔), `--cache-limit`/`YANGPA_CACHE_LIMIT`·`--cache-min-free`/`YANGPA_CACHE_MIN_FREE`(받은 첨부 저장 공간 한도·최소
  디스크 여유의 기본값, 예: `50GB`), `--log`/`YANGPA_LOG`, `--log-file`/`YANGPA_LOG_FILE`(로그를
  표준 오류 대신 파일에, Unix 0600·이어 쓰기. `diag.leakcheck`가 이 파일도 훑음), `--parent-pid`/`YANGPA_PARENT_PID`(이
  프로세스가 끝나면 데몬도 끝남. 데스크톱 앱이 띄울 때 자기 PID를 넘김, `parent.rs`) → 앱 프로세스 지켜보기 시작(Windows는
  여기서 프로세스 핸들을 열어 잠금을 기다리는 사이 PID가 다시 쓰여도 헷갈리지 않음. Unix는 그 PID가 부모면 부모가 바뀌는지,
  아니면 `kill(pid, 0)`을 1초마다 봄) → 데이터 경로 잠금(`yangpad.lock`. 다른
  데몬이 쥐고 있으면 30초까지 끝나기를 기다리고, 그래도 쥐고 있으면 시작 실패) → `Node::start_with_hub` → `yangpa_rpc::serve` →
  Ctrl-C·SIGTERM·`system.shutdown`·지켜보던 앱 프로세스의 끝을 기다려 차례로 닫음. 정리는 20초까지만 기다리고 런타임의 남은 블로킹 작업도 2초까지만
  기다려, 정리가 멈춰도 프로세스가 남아 데이터 경로를 붙잡지 않게 합니다.
- 로컬 API를 프로세스 안에서 부를 때는 `Node::call(&CallCtx, method, params)`. 서버와 같은 범위·사람 전용·공간 제한 검사를 합니다.

## 모듈 소유

코어가 모든 메서드와 노드 간 요청을 연결해 두었고, 다른 작업 영역은 **자기 파일만** 고치면 되도록 고리 함수를 둡니다.
영역 B1~B5, W1~W5, X1~X5를 모두 합친 지금은 자리표시로 남은 메서드가 없습니다(`error::not_implemented`는 `-32603`,
`data.not_implemented`로 남겨 둠). 영역을 넘는 맞물림은 `tests/integration.rs`가 지킵니다.

| 영역 | 파일 | 내용 |
| --- | --- | --- |
| 코어 | `lib.rs`, `main.rs`, `config.rs`, `error.rs`, `node.rs`, `notify.rs` | 시작·종료, 설정, 오류 변환, 알림 |
| 코어 | `identity.rs`, `peers.rs`, `friends.rs` | 신원·기기 목록·프로필, 피어 인증·주소 기록, 연락처 링크·임시 연락처·친구 추가·1:1 공간 |
| 코어 | `author.rs`, `ingest.rs`, `keys.rs`, `spaces.rs`, `keygroup.rs`, `keygroup/{committer,member,relay,shards}.rs`, `messages.rs` | 이벤트 작성·적용, 키(상태 키·뿌리 저장소·잠긴 키 그룹 상태), 공간 런타임, 키 그룹(확정 기기 `KeyUpdate`, 멤버 적용, 놓친 키 중계, 전체 재키 조각 블롭), 메시지 공용 도우미 |
| 코어 | `sync_service.rs`, `state_index.rs`, `devkeys.rs`, `inbound.rs`, `testkit.rs` | 전파·따라잡기·시더 응답, 상태 색인(사본마다의 파생값), 기기 키 표(키 약속 → 상자 키), 노드 간 요청 분배, 시험 도우미 |
| 코어 | `api/mod.rs`, `api/{system,identity,friend,space,message}.rs`, `tests/common`, `tests/core.rs` | 디스패치·토큰 인증, 코어 메서드, 다중 노드 시험 |
| B1 크루 | `crews.rs`, `api/{crew,channel,member,role,invite,emoji}.rs`, `testkit.rs`의 "B1(크루) 시험 도우미" 절, `tests/crews.rs` | 크루·채널 관리·멤버·역할·초대·이모지·크루 아이콘·포럼·비공개 채널·1:1→크루 전환 |
| B2 휘발성·진단 | `volatile.rs`, `diag.rs`, `api/{volatile,diag}.rs`, `testkit/volatile.rs`, `tests/volatile.rs` | 휘발성 채널 전부(RAM 전용), 지표·누출 검사 |
| X1 휘발성 첨부 | `volatile/attach.rs`, `volatile.rs`의 첨부 고리(`try_ready`·`remove_events`·`build_view`·`publish`·`retract`·`send_message`·`wipe`·`start`), `blobs::handle_request` 첫 줄, `blobs.rs`의 내려받기 자리 나눠 쓰기(`borrow_slot`·`return_slot`·`stop_waiting_for_slot`·`with_download_bucket`, `schedule`의 남겨 두기), `api/blob.rs`의 휘발성 갈래, `diag.rs`의 블롭 검사, `tests/volatile_blobs.rs` | 휘발성 채널 첨부(RAM 전용 보내기·받기·내주기·저장 확인) |
| B3 블롭·정책 | `blobs.rs`, `blobs/{fetch,serve,prepare,preview,media,testing}.rs`, `policy.rs`, `api/{blob,seeder}.rs`, `tests/blobs.rs` | 첨부·프로필 이미지·공간 이모지·아이콘 블롭, 링크 미리보기, 시더·피어 설정, 휴면 공간 |
| B4 관리·기기 | `admin.rs`, `notes.rs`, `api/{token,audit,confirm,profile,note}.rs`, `tests/admin.rs` | 토큰·감사·사람 확인, 메모 |
| B4 관리·기기 | `identity.rs`의 "기기 목록·프로필 전파" 절, `api/identity.rs`의 `device.revoke`, `friends.rs`의 "친구 삭제" 절, `api/friend.rs`의 `friend.remove` | 기기 목록 합치기·폐기·새 기기 합류, 프로필 전파, 친구 삭제 |
| Tor 파일 회로 | `filelink.rs`(받는 쪽 길 나누기 `Routes`·`steer` 포함), `blobs/fetch.rs`·`volatile/attach.rs`의 `file_link` 줄, `inbound.rs`의 파일 링크 고리·`FileLink`, `node.rs`의 `filelink::on_disconnected`·`shutdown`, `sync_service`의 `filelink::tick` 줄, `yangpa-net`의 `tor_isolated` | 큰 첨부를 Tor로 받을 때 파일 스웜으로 상대에게 회로 여럿을 따로 열어 조각을 나눠 받기, 상대 파일 링크의 블롭 요청을 그 기기의 주 연결로 답하기. 회로 수 `filelink::circuits`·`set_circuits`·`enabled`(저장 키 `net.file_circuits`, 상대 하나에 0~`MAX_CIRCUITS`(4), 기본 0, 예전 `bool`은 4/0, 줄이면 넘는 회로를 닫음), `net.file_circuits`·`net.set_file_circuits`(`api/settings.rs`) |
| Tor 브리지 | `bridges.rs`, `api/settings.rs`의 `bridges`·`set_bridges`, `tor.rs`의 `tor_config`(시작할 때 브리지), `node.rs`의 `bridges::start`, `yangpa-net`의 `TorBridges`·`set_tor_bridges`·`tor_bootstrap`·`check_bridge_line` | 저장 키 `net.bridges`(없으면 한 번도 정하지 않음), lyrebird 찾기(exe 옆 `pt`·exe 옆·PATH, Windows는 PATH 빼고), 떠 있는 Arti 다시 설정, 자동 Snowflake(진행률 90초 멈춤 → 3분 시험 → 저장·`net.bridges` 알림, 못 들어가면 되돌리고 30분 뒤) |
| 신뢰 스위치 | `trust.rs`, `api/trust.rs`, `p2p.rs`의 출구 스웜·`want_for`·`link_ok`·`carries_normal`·`request`·STUN·홀펀칭, `friends::remove`의 `trust::forget_user` 줄, `crews::on_state_applied`의 `trust::on_state_applied` 줄, `yangpa-cli`의 `yangpa trust` | 사람·크루 신뢰(`trust.users`·`trust.spaces`), `SpaceView.trusted`, 연결 방식(`ConnectionMode`), 단방향(출구) 연결, 공유기 너머 직접 연결 |
| B5 통화·망 | `voice.rs`, `tor.rs`, `stun.rs`, `locator.rs`, `api/{voice,settings}.rs`, `testkit/net.rs`, `tests/net_features.rs`, `yangpa-net`의 부트스트랩 | 통화 신호·프레임 키·SFU 기준, Tor 전용 연결 설정·예전 설정 지우기, STUN 서버 목록, 위치 기록(DHT) |
| X2 SFU 통화 | `sfu.rs`, `voice.rs`의 토폴로지 결정·호스트 후보·`Sfu*` 신호 절, `config.rs`의 `SfuOptions`, `main.rs`의 `--sfu-*`, `voice::enforce_policy`·`mark_wanted`, `stun::stun_servers`(서버 반사 후보), `testkit/sfu.rs`, `tests/sfu.rs`, `crates/yangpa-sfu` | 데몬 안 SFU 호스트, 호스트 선출·히스테리시스·넘겨주기, Tor로 도는 기기에서 맡지 않기 |
| W2 모더레이션 | `moderation.rs`, `api/moderation.rs`, `crews.rs`의 밴 거절 줄(`handle_redeem_invite`·`join`)과 `on_decoded`·`on_resolved` 첫 줄, `spaces::discard_space_locked`의 `moderation::forget_space` 줄, `identity::wipe`의 `moderation::wipe_all` 줄, `volatile::author_for_api`, `notify.rs`의 거르개, `testkit.rs` 끝의 모더레이션 도우미, `tests/moderation.rs` | 밴·차단·신고 |
| 통합 | `tests/integration.rs` | 여러 영역이 함께 맞물리는 다중 노드 시나리오(휘발성 첨부·저장 공간 정리·보관 노드 사이의 경계 포함) |
| X3 보관 전용 노드 | `archive.rs`, `archive/{host,member}.rs`, `api/archive.rs`, `testkit/archive.rs`, `tests/archive.rs`, `config.rs`의 `ArchiveOptions`, `main.rs`의 `--archive`·`--archive-quota`, `yangpa-core`의 `archive.rs`, `yangpa-sync`의 `archive.rs` | 보관 노드 지정·해제·명단, 보관 노드 쪽 받기·내주기·첨부·해제, 멤버 쪽 밀어 보내기·첨부 알림·삭제 반영·커버리지 |
| W4 Tor·성능 | `tor.rs`의 onion 연락처, `blobs/preview.rs`의 Arti 경유 조회, `blobs/fetch.rs`의 Tor 받기 한도, `sync_service::gossip_verdict`와 `ingest.rs`의 `rejected_by_state`, `locator.rs`의 시험용 기록 심기, `testkit/net.rs`, `tests/{tor_leak,perf}.rs`, `yangpa-net`의 검사 API·gossip 출처 검사, `crates/yangpa-bench` | Tor 누출 시험, Tor 경유 미리보기, 상태로 갈린 gossip 거부는 감점 안 함, 성능 목표·명령 수 회귀 검사 |
| X4 저장 공간·미디어 | `blobs/{cache,prefetch,hints}.rs`, `api/storage.rs`, `api/crew.rs`의 `crew.committer_candidates`, `tests/storage.rs` | 받은 첨부 LRU 정리·고정, 헤드리스 미리 받기, 앱이 만든 동영상·음성 썸네일·길이 검사, 확정 기기 후보 |
| P2P 전용 채널 | `p2p.rs`, `api/channel.rs`의 `p2p: true` 만들기·`channel.set_p2p`·`p2p_join`·`p2p_leave`, `crews::on_state_applied` 첫 줄, 각 영역의 경로 검사 줄(`ingest`·`sync_service`·`blobs`·`volatile`·`voice`·`author`), `testkit.rs`의 `p2p_running` | 기기별 참여 동의(`p2p.joined`), 채널 보기의 `p2p`·`p2p_joined`, 참여한 기기끼리만 쓰는 두 번째 스웜(P2P 스웜)·참여 안내(`P2pHello`)·P2P 인증·길 나누기 |
| 익명 가명 | `anon.rs`, `messages::mask`·`notify_view`, `api/message.rs`의 `forum_post_view`, `voice::view_of`, `typing::handle`의 `anon_name` | 익명 글·게시글·통화 참가자·글 쓰는 중·OS 알림의 가명(`anon_name`) |
| 글 쓰는 중 | `typing.rs`, `node.rs`의 `typing`, `api/channel.rs`의 `channel.typing`, `inbound.rs`·`p2p::handle_inbound`의 `Typing` 줄 | 보내기 간격(채널마다 2초)·받을 기기, 받은 `Typing` 확인과 `channel.typing` 알림(기기·채널마다 1초), RAM만 |
| 붙여넣은 첨부 | `paste.rs`, `node.rs`의 `pastes`, `api/blob.rs`의 `blob.paste`·`blob.paste_discard`, `blobs/prepare.rs`의 `Origin`(경로·붙여넣기)·`paste::Source`, `volatile/attach.rs`의 `Origin` 줄 | 경로 없는 첨부 바이트를 0으로 덮는 RAM 버퍼에 두고(30분, 합쳐 512MiB·64개) 일반·휘발성 보내기가 빌려 씀 |
| 채널 첨부 한도 | `api/channel.rs`의 `attach_limit`·`check_attach_limit`, `api/space.rs`의 `ChannelView.attach_limit`, `blobs/prepare.rs`의 `send_limit`·`size_text`, `blobs::on_message_applied`·`prefetch::backfill`의 한도 줄, `volatile/attach.rs`의 `channel_max`, `yangpa-sync`의 `channel_attach_limit_set` | 크루 관리자가 정한 채널 첨부 한도(`ChannelAttachLimitSet`, 0은 첨부 금지 `check_attach_allowed`, 크루의 휘발성 채널은 만들 때 10MiB): 보낼 때 이 기기 설정과 작은 쪽, 넘는 첨부는 자동으로 받지 않음 |
| 채널 수정·삭제 금지 | `api/channel.rs`의 `no_edit`·`no_delete`·`check_edit_rules`, `api/message.rs`의 `check_editable`·`check_deletable`(휘발성 `volatile.rs`도 씀), `yangpa-sync`의 `channel_edit_rules_set`·`authorize_message` | 크루 관리자가 정한 채널 수정·삭제 금지(`ChannelEditRulesSet`): 보내기 전에 거절하고, 받는 쪽도 권한 검사에서 거름 |
| W3 견고성 | `identity.rs`의 기기 목록 판정·인증 거절·지우기 절, `keygroup.rs`의 표 넘기기·크루 밖 확정 기기 인계·묶음 창, `notes.rs`, `volatile.rs`의 `deliver_key`, `api/crew.rs`의 `crew.set_committer`, `tests/robustness.rs` | 폐기·시드만 가진 기기 막기·확정 기기 인계·가입 묶기·키 갱신 재전송·메모 나누기 |
| 기기 연결 | `devlink.rs`, `api/identity.rs`의 `device.link.*`, `identity.rs`의 `install_linked`·`add_device`·`spawn_first_sync`, `inbound.rs`의 `DeviceLink`·`DeviceLinkPoll` 줄, `yangpa-core`의 `links.rs` `DeviceLink`·`wire.rs` `device_link_code`, `tests/net_features.rs`의 기기 연결 시험 | 새 기기를 내 기기가 띄운 링크(QR)와 6자리 번호로 들이기 |

### 코어가 부르는 고리

| 고리 | 부르는 곳·때 |
| --- | --- |
| `<영역>::start(&Arc<Node>)` | `Node::start` 끝(공간을 다시 세운 뒤, 네트워크 이벤트를 받기 전) |
| `crews::on_state_applied(node, space, old, new)` | 해소된 공간 상태가 바뀔 때마다(적용 잠금 안 — 오래 걸리면 `spawn_detached`) |
| `keygroup::on_state_changed(node, space)` | `ingest::after_unlock`: 상태가 바뀐 뒤(내가 확정 기기면 묶음 창 뒤 한 바퀴, 아니면 표 넘기기·크루 밖 확정 기기 인계만) |
| `crews::handle_redeem_invite` | 들어온 `RedeemInvite`(인증 전에도 옴) |
| `crews::on_decoded`, `crews::on_resolved` | 상태 레인 이벤트를 해소기에 넣기 직전·해소 직후(적용 잠금 안). 해소 결과의 적용·해제(`applied`·`unapplied`)로 포럼 게시글 변경 반영, 가입 이벤트의 서명된 프로필·기기 목록 저장 |
| `crews::on_forum_post_applied` | `ForumPost` 보기 행이 막 생겼을 때(먼저 온 게시글 변경 적용) |
| `crews::trusted_for_join`, `crews::join_peer_ok` | `peers::is_trusted`, `sync_service`의 `PushEvents`·따라잡기: 상태를 풀기 전인 가입 대기 공간의 받아 준 노드·공간을 만든 사용자 기기 |
| `volatile::on_inbound_event(node, space, state, ev, from)` | 휘발성 채널 스트림 이벤트·`KeyRef::Volatile` 이벤트. 저장하지 않고 넘김. 판정은 gossip 전달 여부 |
| `volatile::is_direct_only(state, ev)` | `sync_service::on_gossip`: 하나라도 참이면 그 gossip 묶음을 적용·전달하지 않고 거부(휘발성은 직접 전송만) |
| `volatile::{send,list,edit,delete}_message`, `volatile::react` | 휘발성 채널에 대한 `message.*` |
| `volatile::mark_read` | 휘발성 채널의 `channel.mark_read`(읽음 위치는 RAM) |
| `volatile::wipe_all` | `Node::shutdown`(적용 잠금을 쥔 뒤) |
| `checkpoint::set_floor` | 해소기가 체크포인트를 받아들인 직후(적용 잠금 안): 경계 F를 고침 |
| `checkpoint::on_accepted`, `checkpoint::resign` | `ingest::after_unlock`: 받아들인 체크포인트의 스냅숏·사슬·바탕 기록, 내 늦은 이벤트(`LateEvent`·`RetiredStateKey`) 다시 내기 |
| `checkpoint::restore`, `checkpoint::forget_space` | `spaces::load_all`(시작), 공간 지우기 |
| `checkpoint::prepare_join`, `try_install`, `retry_prepare` | `crews::join`(가입 묶음 적용 전·후)과 가입 감시 작업 |
| `checkpoint::floor`, `is_below`, `note_below` | `author`(F 아래 머리를 부모에서 뺌), `sync_service`(따라잡기·지문 비교를 F부터) |
| `volatile::handle_snapshot`, `volatile::handle_presence`, `volatile::handle_key_wrap` | 들어온 `VolatileSnapshot`·`VolatilePresence`·`VolatileKeyWrap`(키 감싸기는 동기, 적용 잠금 밖) |
| `volatile::filter_notification(node, auth, n)` | 로컬 API 연결별 알림 거르기(`ApiHandler::filter_notification`) |
| `volatile::fill_channel_view`, `voice::fill_channel_view`, `p2p::fill_channel_view` | `channel.list`·`space.get`의 채널 보기마다(입장 여부·참가자·API 읽기 허용·P2P 전용 여부와 참여를 채움, 휘발성·음성·P2P 순) |
| `p2p::on_state_applied(node, space, old, new)` | `crews::on_state_applied` 맨 앞(적용 잠금 안): P2P 전용이 아니게 된 채널·멤버가 아니게 된 공간의 참여 기록 지우기 |
| `volatile::on_peer_disconnected`, `voice::on_peer_disconnected` | 피어 연결이 끊길 때 |
| `blobs::prepare_attachments`, `blobs::link_previews` | `message.send`(첨부는 ATTACH_FILES가 있을 때, 미리보기는 채널 설정과 EMBED_LINKS가 허용할 때) |
| `blobs::discard_prepared(node, blobs)` | `message.send`가 첨부를 준비한 뒤 발행에 실패했을 때(쓰임이 없는 블롭을 지움) |
| `blobs::import_profile_image` | `profile.set`의 `avatar_path`·`banner_path` |
| `blobs::profile_image_key(node, blob)` | `identity::set_profile`이 서명된 프로필에 이미지 키를 실을 때 |
| `blobs::import_bytes(node, space, bytes)` | 메모리의 바이트를 블롭으로(B1 공간 이모지). `(BlobId, 키)` |
| `blobs::import_space_image(node, space, path)` | `crew.update`의 `icon_path`: 그 크루 공간에 등록한 이미지 블롭과 키(`SpaceSettings::icon_key`에 실음) |
| `blobs::is_downloaded` | 메시지 보기의 `downloaded` |
| `blobs::on_message_applied(node, space, channel, id, header, body)` | `Message`·첨부가 있는 `ForumPost`가 보기 행에 들어간 뒤(적용 잠금 안. 쓰임 기록·자동 내려받기 판단) |
| `blobs::handle_request` | 들어온 `BlobManifest`·`BlobHave`·`BlobChunk`(인증·멤버 확인은 이 함수 몫) |
| `volatile::attach::serve(node, peer, req)` | `blobs::handle_request` 맨 앞: RAM에 있는 휘발성 첨부면 답하고(디스크에도 있는 블롭은 빼고), 아니면 `None`(일반 경로) |
| `volatile::attach::find(node, space, blob, key_from)` | `api/blob.rs`의 `blob.status`·`fetch`(`key_from` 없음)·`read`·`save` 맨 앞: 휘발성 첨부면 `volatile::attach`가 처리 |
| `blobs::borrow_slot(node)` / `return_slot` / `stop_waiting_for_slot` | `volatile::attach`의 대기열 일꾼: 일반 첨부와 같은 동시 내려받기 한도(`max_active_downloads`, Tor로 도는 기기면 1. 빠른 연결로만 받는 일반 첨부는 따로 셈)에서 자리를 빌리고 돌려줌. 자리를 기다리면 `schedule`이 빈자리 하나를 남겨 두고 `volatile::attach::kick`으로 깨움. 직접 연결 몫을 돌려줄 때(`return_slot(fast)`)도 바로 깨움(휘발성 대기는 `lend_wanted`에 적지 않으므로) |
| `blobs::chunk_window(node, peers)` / `Pacer` | `blobs::fetch`·`volatile::attach`의 조각 요청 겹침: 받을 피어가 모두 Tor를 거치지 않는 직접 연결(`p2p::data_goes_direct`, 단방향 제외)이면 `ChunkWindow::Direct`(`Pacer`가 받은 바이트로 속도를 재어 `yangpa_sync::swarm::direct_window`로 정함), 아니면 Tor 한도 또는 기본값(`Fixed`) |
| `blobs::chunk_ticket(node, peer)` | 조각 요청 하나를 보내기 전에 받는 자리(모든 내려받기 합계, 피어당 24개). 답이 오면 놓음. 못 받거나 `NetError::Busy`면 `LOCAL_BUSY_MS` 쉼 |
| `sync_service::serving(node, device)` | 시더 요청 처리(동기화 `GetEvents`·블롭·휘발성 첨부·보관 노드): 처리하는 동안 잡아 두고, 함께 처리 중인 다른 기기 수를 `SeedRequest::in_flight`로 씀(동시 전송 수는 기기 단위) |
| `blobs::with_download_bucket(node, space, now, f)` | `volatile::attach`의 조각 요청: 일반 첨부와 같은 수신 대역폭 버킷(공간 피어 설정이 따로 있으면 그 공간, 아니면 전역) |
| `policy::seeder_policy`, `policy::channel_opened` | 시더 요청 검사(`check_seed_request`의 입력) |
| `policy::mark_channel_opened` | `message.send`·`message.list`(열어 본 채널 기록, 활동, 미룬 자동 내려받기 시작) |
| `policy::is_dormant(&Arc<Node>, space)` | 주기 따라잡기가 공간마다(휴면이면 건너뜀. 기한이 지났으면 여기서 휴면으로 바꾸고, 휴면이 되면 안 되는 공간이 표시돼 있으면 깨움) |
| `policy::is_marked_dormant` | 연결 직후 따라잡기·gossip 구독(`refresh_space_peers`)이 휴면 공간을 뺄 때 |
| `policy::on_push` | 받은 `PushEvents`(직접 전파 = 활동, 휴면이면 깨움). gossip은 활동으로 세지 않음 |
| `voice::handle_signal` | 들어온 `Signal` |
| `voice::start` | 음성 영역은 `node.notify`를 구독해 `space.updated`마다(놓치면 전부, 20초 주기로도) 통화 참가 자격을 다시 봄(추방·기기 폐기·권한 변경·채널 삭제). 코어가 따로 부르는 고리는 없음 |
| `admin::before_call`, `admin::after_call` | 모든 API 호출 앞뒤(자동화 토큰의 파괴적 작업 → 확인 대기열, 감사 로그). 인증하지 않은 호출은 남기지 않음 |
| `admin::on_server_call` (`RpcHandler::on_server_call`) | 서버가 직접 답한 호출(`auth.hello`, `events.subscribe`/`unsubscribe`, 범위 검사 거절)의 감사 로그 |
| `admin::is_revoked` | `ApiHandler::filter_notification`: 폐기한 토큰의 열린 연결에는 알림을 보내지 않음 |
| `admin::confirm_when_others` | 남의 메시지 삭제처럼 "남에게 하면 파괴적"인 작업(저장 공간 한도를 줄이는 `blob.cache_set_limit` 포함). 인자의 `{space, channel}`이 휘발성 채널이면(상태를 모르면 조심해서 휘발성으로 봄) 요청을 RAM에만 둠: B2의 휘발성 `message.delete`도 그대로 부르면 됨 |
| `identity::observe_state_body(node, body)` | 적용 경로가 상태 이벤트 본문을 연 직후(해소 전, 적용 잠금 안): 실린 서명 기기 목록·프로필 반영 |
| `identity::on_state_applied(node, space, old, new)` | `crews::on_state_applied` 다음(적용 잠금 안): 내 기기가 알려 준 공간이 해소되면 친구 기록 되살리기, 1:1 공간에서 나간 친구 지우기(`friends::on_state_applied`) |
| `identity::on_authenticated(node, peer, user, device)` | 인증을 건 쪽이 인증을 마친 직후: 프로필 교환, 내 기기면 기기 목록·메모 맞추기, 전하지 못한 친구 삭제 전달 |
| `identity::on_auth_accepted(node, peer, user)` | 상대가 건 인증을 받아 준 직후(`peers::handle_auth`): 전하지 못한 친구 삭제 전달 |
| `friends::has_outgoing_request`, `friends::has_pending_leave` | `peers::is_trusted`: 친구·멤버 말고도 받아 줄 사용자(내가 연락처 링크를 쓴 사람, 친구 삭제를 아직 전하지 못한 사용자, 30일까지) |
| `identity::is_adopting(node, space, user)` | 상태가 없는 공간의 `PushEvents`·따라잡기(`catch_up_peer`)를 그 사용자와 할지(내 기기가 알려 준 공간: 내 기기만) |
| `identity::adoption_tick` | 주기 작업(`spawn_periodic`): 30일 안에 해소되지 않은 받아들인 공간 포기 |
| `identity::handle_social` | 들어온 `Social(Profile / Devices / Notes / Contact)`(인증된 피어만, 메모는 `notes::handle_notes`, 주소는 `peers::record_contact`) |
| `peers::on_status` | 스웜 상태 알림마다(`spawn_net_loop`): 내 링크용 주소가 바뀌었으면 인증된 피어와 다시 교환 |
| `peers::spawn_redial` | 시작할 때 한 번: 끊긴 친구·내 기기·작은 크루 멤버 기기 다시 걸기와 보내지 못한 연락처 링크 요청(`friends::retry_pending_redeems`) |
| `friends::spawn_contact_expiry` | 시작할 때 한 번(마지막): 5초마다 `friends::expire_due`로 기한이 지난 임시 연락처를 만료 |
| `friends::on_peer_authed(node, user)` | 인증이 끝난 피어마다(`peers::remember`): 그 사용자에게 아직 전하지 못한 친구 추가 요청(`Social(Propose)`)을 보냄 |
| `friends::on_expired_by_peer` | 내 `Auth`가 `Expired`로 거절됐을 때(`peers::ensure_auth`·`reconfirm_auth`): 이쪽 임시 연락처도 만료. 인증을 보낸 뒤 기록이 바뀌었으면(새 링크로 되살아남) 늦은 거절이라 두지 않음 |
| `friends::space_deactivated(node, space)` | `author::publish*`(쓰기 거절 `1003`), `sync_service`의 전파·따라잡기·시더 응답, 다시 걸기 대상, 신뢰 판정(`peers::is_trusted`), P2P 참여 판정: 만료된 연락처와의 1:1 공간은 쓰지도 주고받지도 않음 |
| `blobs::on_profile_changed(node, user, profile)` | `identity::accept_profile`이 저장한 판이 바뀌었을 때: 프로필 이미지 색인, 그 사람과의 1:1 공간이면 새 아바타·배너를 받을 목록에 |
| `tor::net_config(cfg, store)` | 네트워크를 띄우기 직전(`Node::start`). 예전 모드 설정과 없어진 망 설정을 지우고(`migrate_legacy`, `LEGACY_SETTINGS`) Tor 스웜(기본) 또는 직접 연결 스웜(`--direct`) 설정을 만듦(부트스트랩·공개 노드는 시작 인자만). Tor를 준비할 수 없으면 오류로 시작을 멈춤. 실행 중에 모드를 바꾸는 길은 없음 |
| `tor::status` | `system.status`, `net.status` |
| `stun::stun_servers(node)`, `stun::view`, `stun::set` | `sfu.rs`의 서버 반사 후보 모으기(직접 연결 경로가 공유기 밖 주소를 알아낼 때), `net.stun_servers`·`net.set_stun_servers`(`api/settings.rs`). 저장 키 `net.stun_servers`, 없으면 `NodeConfig.default_stun_servers`(Google 두 곳·Cloudflare 한 곳, 시험은 빈 목록). 항목은 `stun::normalize`로 `host:port`(`stun:` 떼기, 기본 포트 3478, 8개·호스트 253자까지, TURN·자격 증명 거절) |
| `stun::normalize_crew`, `stun::set_crew_use`, `stun::fill_space_view`, `stun::crew_stun_all`, `stun::call_ice` | 크루 STUN 서버: `crew.set_servers`·`crew.use_servers`(`api/crew.rs`, 쓰기로 한 크루는 설정 `turn.crews`, 키 이름은 TURN이 있던 때 그대로), `space.get`의 `crew_servers`·`crew_stun`·`use_crew_servers`(`api/space.rs`), 직접 스웜의 공인 주소 묻기(`p2p.rs`, 내 목록 뒤에 쓰기로 한 모든 크루의 STUN), `voice.join`의 `ice_servers`(`voice.rs`, 내 STUN 뒤에 그 크루의 STUN을 붙인 `stun:` 서버 하나). 권한 `use_crew_stun`이 있어야 씀 |
| `locator::lookup` | 전파할 기기의 주소를 모를 때, 기록된 주소로 보내기가 실패하고 연결도 없을 때(`sync_service::push_to`), 다시 걸기의 마지막 단계(`peers::redial_device`). 모두 `peers::reach_found`를 거쳐 찾은 노드에 걸고 인증해 찾던 기기일 때만 씀. 위치 기록으로 못 찾으면 멤버 소개(`FindDevice`, `locator::handle_find_device`가 답함), 물어볼 멤버가 없으면 전에 소개받아 둔 노드(`peer.intro.<기기 ID>`). 찾은 노드로 다시 보내는 데 성공하면 `locator::note_reroute`(시험용 횟수) |
| `peers::handle_check_join` | 받은 `CheckJoin`(보내는 쪽은 `peers::reach`가 찾은 기기에게 거절당했을 때 `ask_check_join`, 내 상태가 그 기기를 담은 크루만 지목). 지목된 크루 중 내가 멤버인 것이 있고 기기마다 10분·전체 8번 안이면: 그 기기에 내 인증을 보내고 `sync_service::pull_space_stream`으로 그 크루들의 공간 스트림만 받은 뒤, 상태가 그 기기를 담으면 `ensure_auth`·`catch_up_peer` |
| `locator::on_peer_authed` | 인증이 끝난 피어마다(`peers::remember` 끝): identify 수신 주소를 `peer_cache`에 기록하고 `voice::on_peer_authed`(참가 중인 통화를 다시 알림, 상대가 아직 이쪽을 인증하지 않아 거절하면 잠깐 뒤 다시)를 부름 |
| `p2p::start`, `p2p::shutdown` | `Node::start`의 영역 `start` 고리 끝(참여한 채널·신뢰가 있으면 직접 스웜을 띄움), `Node::shutdown`(작업 중지 뒤, 주 스웜보다 먼저 내림) |
| `p2p::redial_soon` | `p2p::on_disconnected`(인증된 P2P 연결이 끊김): 그 기기에 1·5·15·45초 뒤 다시 걸기(`connect_one`, 이어지면 멈춤). `p2p::learn`은 상대 주소가 바뀌면 걸었던 기록(`dialed`)을 지워 바로 다시 겁니다 |
| `p2p::reconcile` | `p2p::join`·`leave`(`channel.p2p_join`·`p2p_leave`), `trust::set_user`·`set_space`, `p2p::on_state_applied`(바뀐 공간이 내 참여 기록·받은 안내·신뢰한 크루에 걸려 있을 때), `p2p::start`: 직접·출구 스웜을 필요한 대로 띄우고 내림 → 바라지 않게 된 P2P 인증 지우기(`prune`) → 안내를 다시 맞춤 → 바라는 연결 걸기(`connect_all`) → `voice::leave_blocked`(막힌 통화에서 나옴) |
| `trust::trusts(node, user)` | `p2p`의 `hello_for`(주소·`trusted`를 싣는지), `want_for`, `link_ok`, `carries_normal`·`send_normal`: 사람 스위치이거나 함께 멤버인 크루 중 신뢰한 크루가 있음 |
| `trust::on_state_applied` | `crews::on_state_applied`: 내가 나간 크루의 신뢰를 지우고, 신뢰한 크루가 바뀌었으면 `p2p::reconcile` |
| `trust::forget_user` | `friends::remove`(친구·임시 연락처 끊기, 만료된 연락처 지우기 둘 다): 그 사람의 신뢰를 끔 |
| `p2p::connection_mode(node, user)` | `trust.get`의 `connection`: 그 사람 기기와의 P2P 인증 연결(직접·출구 어느 쪽인지)과 Tor 연결 중 가장 빠른 것 |
| `p2p::handle_punch` | 들어온 `P2pPunch`(`inbound.rs`, Tor로 인증된 피어만): 배우고, 막 고친 내 노드로 답하고, 역할을 바꾼 다이얼로 구멍을 뚫음 |
| `p2p::on_peer_authed(node, peer, user)` | Tor로 인증이 끝난 피어마다(`peers::remember`): 그 기기에 보낸 안내 기록을 지우고 맞춘 안내(`P2pHello`)를 다시 보냄 |
| `p2p::handle_hello` | 들어온 `P2pHello`(`inbound.rs`. Tor로 인증된 피어만) |
| `p2p::tick` | 주기 작업(`sync_service::spawn_periodic`): 전하지 못한 안내 다시 보내기, P2P 피어와 함께 참여한 공간 따라잡기 |
| `p2p::may_author(node, space, channel)` | `author::publish*`: 참여하지 않은 P2P 전용 채널의 채널 스트림에는 쓰지 않음(`1003`) |
| `p2p::accepts`, `p2p::route_ok` | `ingest`(받은 묶음에서 잘못된 길로 온 이벤트 버림, gossip 포함), `sync_service`의 시더 응답(`NotFound`), `blobs/serve.rs`의 `authorize`, `volatile`의 직접 전달, `volatile/attach.rs`의 내주기, `typing::handle` |
| `p2p::split`, `p2p::push` | `sync_service::propagate`: P2P 이벤트를 떼어 함께 참여한 P2P 피어에게만 `PushEvents`로 |
| `p2p::shared_channels`, `p2p::is_p2p_peer` | `sync_service::catch_up_peer`: P2P 피어와는 함께 참여한 채널의 채널 스트림만 맞춤 |
| `p2p::channel_peers` | `blobs::member_peers`(P2P 전용 채널 첨부의 스웜 피어), `volatile`의 온라인 참가자, `typing`의 받을 기기 |
| `p2p::request(node, peer, req)` | `blobs/fetch.rs`·`blobs.rs`·`volatile.rs`·`volatile/attach.rs`(매니페스트 묻기 포함)·`sync_service.rs`·`typing.rs`: 받는 피어가 P2P 노드면 그 노드가 있는 P2P 쪽 스웜으로. Tor 피어로 가는 데이터 요청은 그 기기와 일반 내용을 나르는 P2P 연결(`send_normal`)이 있으면 그쪽으로 먼저 보내고, 실패하면 Tor로 |
| `p2p::p2p_joined(node, space, channel)` | `voice`(통화 참가 자격·ICE 정책), `volatile`(입장 허용): 이 기기가 그 P2P 전용 채널에 참여했는지 |
| `diag::on_ingest(node, accepted, sent)` | 적용 묶음마다(받아들인 ID, 피어가 보낸 메시지 레인 이벤트의 작성 시각 `diag::sent_times`) |
| `moderation::on_decoded`, `moderation::on_resolved` | `crews::on_decoded`·`crews::on_resolved` 맨 앞(적용 잠금 안): `delete_since`가 실린 밴을 기억했다가 적용되면 그 사용자가 `delete_since`부터 밴 시각까지 쓴 메시지를 지움 표시하고 `message.updated`. 밴 이벤트마다 한 번만(`mod.bandel.<공간>.<이벤트>`, 시작 때 다시 세울 때 되풀이하지 않음) |
| `moderation::forget_space` | `spaces::discard_space_locked`(탈퇴·공간 정리): 그 공간의 받은 신고·보낸 기록·숨김·밴 삭제 기록·대기열(RAM 것 포함)을 지움 |
| `moderation::is_blocked` | `messages::view_of`(`author_blocked`), `member.list`·`space.get`(`blocked`), `blobs::on_message_applied`(차단한 사람의 첨부는 자동으로 받지 않음) |
| `moderation::is_author_blocked` | 사람 차단이나 그 글을 쓴 기기의 기기 차단(`mod.blocked_dev.<기기>`): `messages::view_of`·`message.list`(`author_blocked`), 알림 거르개, 첨부 자동 받기(`blobs`·`prefetch`·휘발성 첨부), 입력 표시 |
| `moderation::is_device_blocked` | `peers::is_trusted` 맨 앞(차단한 기기는 어떤 관계로도 믿지 않음). `block_device`는 그 기기의 인증을 잊음(`forget_devices`) |
| `identity::forget_old_otp(store)` | `Node::start`: 없어진 OTP가 남긴 비밀(`otp.mine`, `otp.escrow.<사용자>`)을 지움 |
| `moderation::blocked_users` | `space.list`·`space.get`·`channel.list`의 안 읽은 수(`Store::unread_counts_excluding`) |
| `moderation::is_hidden`, `moderation::has_hidden` | `message.list`(휘발성 포함)·`message.search`에서 숨긴 메시지 빼기(검색은 전체 수에서도 뺌). 목록은 뺀 만큼 더 오래된 쪽에서 채워 `limit`보다 짧은 페이지가 기록의 시작일 때만 나오게 함 |
| `moderation::withholds` | `sync_service::check`: 시더 정책을 통과한 메시지 레인 요청을 차단한 사용자에게는 `NotSeeded`로 |
| `moderation::silent_friend_refusal` | `friends::handle_redeem`: 차단한 사용자의 연락처 링크 사용(링크 사용·응답은 보통과 같고 요청·임시 연락처·알림을 남기지 않음) |
| `moderation::handle_request` | 들어온 `Moderation(Report / Resolved)` |
| `Notifier::set_filter` | `moderation::start`가 한 번 등록: 차단한 사람의 `message.new`, 숨긴 메시지의 `message.new`·`message.updated`를 모든 구독자에게서 거르고, `message.updated`의 `author_blocked`를 채움(휘발성 보기는 차단을 모르고 만들어짐) |
| `archive::route(node, peer, req)` | `inbound.rs`의 신원 확인 뒤 분배 전: 받아 둔 공간(내가 멤버가 아님)의 동기화·블롭 요청은 보관 노드 쪽이 답하고, 보관 전용 연결(보관 관계로만 믿는 상대: 멤버 쪽에서는 지정된 보관 기기, 보관 노드 쪽에서는 명단의 멤버 기기. 내 기기·친구·함께 있는 공간의 멤버는 아님)에서는 `Ping`·`Auth`·`Archive`·동기화 읽기·블롭 요청만 넘김(나머지 `Forbidden`) |
| `archive::handle_request` | 들어온 `Archive(Designate / Roster / BlobRefs / Prune / Release)`(지정·해제는 인증 전에도 옴) |
| `archive::trusts` | `peers::is_trusted`: 상태에 지정된 보관 기기(멤버 쪽), 받아 둔 공간 명단의 멤버 기기(보관 노드 쪽) |
| `archive::archive_only` | `peers::{ensure_auth, probe_auth, handle_auth}`·`identity::push_profile`: 보관 전용 연결이면 `identity::on_authenticated`·`on_auth_accepted`(프로필 교환 등)와 프로필 보내기를 건너뜀 |
| `archive::forward(node, space, events)` | `sync_service::propagate` 맨 앞: 내가 쓴 보관 범위 이벤트(키 그룹 스트림은 빼고)를 연결된 보관 노드에 `PushEvents`로 |
| `archive::on_peer_authed`, `archive::is_active` | `sync_service::catch_up_peer`: 보관 노드 쪽은 그 멤버 기기에게서 받아 둔 공간을 당겨 오고, 멤버 쪽은 지정된 보관 노드도 따라잡기 상대로 넣음 |
| `archive::check_seed` | `sync_service::check` 맨 앞: 요청자가 지정된 보관 기기면 보관 규칙(`yangpa_sync::archive::check_archive_seed`)으로 판단 |
| `archive::archive_peers` | `blobs::member_peers`(블롭 스웜 피어에 보관 노드를 더함) |
| `archive::is_active` | `blobs/serve.rs`: 보관 노드에게 보관 범위 채널의 첨부는 주고 아이콘·프로필 이미지는 주지 않음 |
| `archive::coverage` | `api/seeder.rs`의 `seed.coverage`(`online_archives`) |
| `blobs::referenced_blobs` | `archive/member.rs`가 보관 노드에 알릴 첨부를 훑을 때 |

### 다른 영역이 할 일

- **노드 상태 더하기**: 자기 모듈의 상태 구조체(`CrewState`, `VolatileState` …, `Default`)에 필드를 넣습니다. `node.rs`는 고치지 않습니다.
  오래 도는 작업은 `start`에서 `node.tasks.spawn(...)`(종료 때 멈춤), 짧은 일은 `node.spawn_detached(|node| async move { … })`.
- **이벤트 작성**: 상태 이벤트(크루·채널·역할 …)는

  ```rust
  let l = ingest::lock_space(node, space).await;
  let (ev, report) = author::publish_locked(node, &l, space, None, &body, KeyChoice::State, &[])?;
  drop(l);
  ingest::after_unlock(node, space, &report);   // 상태가 바뀌었으면 커밋 확정
  sync_service::propagate(node, space, vec![ev]);
  ```

  한 개만 낼 때는 `author::publish(node, space, channel, &body, key)`. 본문 키는 `KeyChoice`(지금 상태 키 `State`, 지정한 상태 키 `StateWith(ID, 키)`(공간 첫 키·키
  그룹 갱신·그룹 상태 키로 봉인하는 묶음), 채널 뿌리 `HistoryWith(ID, 뿌리)`, 휘발성 세션 키 `Volatile(세대, 키)`, 봉인 `Sealed`).
  평문은 없습니다. 만든 이벤트는 받은 이벤트와 같은 적용 경로를 지나며(`KeyRef::Volatile`이면
  저장하지 않고 `volatile::on_inbound_event(.., None)`), 권한 검사에서 거부되면 `Error::Forbidden`.
- **키 그룹 상태 저장**: 멤버 상태(`SpaceCore::keys`)는 잠긴 키 저장소의 그룹별 비밀값(`kg.mg.<공간>.<그룹>`, 목록
  `kg.mi.<공간>`, 옛 `kg.m.<공간>`은 읽어 옮긴 뒤 지움), 확정 기기 표는 그룹마다 두 칸(`kg.tg.<공간>.<그룹>.<0|1>`, 목록
  `kg.ti.<공간>`)이고, `keys::save_member_row`·`committer`의 `stage`·`flush`·`save_table`로만 씁니다(로그·설정·캐시로 가지 않음). 그룹을 바꾸는 갱신은
  `keygroup::committer::round_locked`(표의 복제본에 하고 해소기·그룹 사슬이 받아들인 뒤에만 표를 바꿈)를 쓰세요.
- **공간 지우기**: `spaces::discard_space_locked(node, &l, space)`(런타임·이벤트·키·설정·키 그룹 상태·표·gossip 구독).
- **고리 쓰기**: 위 표의 함수 본문만 채웁니다. 적용 잠금 안에서 불리는 고리(`on_state_applied`, `on_inbound_event`,
  `on_message_applied`)는 다시 잠그거나 기다리지 말고 일을 넘기세요.
- **메서드 구현**: `api/<영역>.rs`의 함수 본문을 채웁니다. 디스패치 표(`api/mod.rs`의 `routes!`)는 이미 모든 `METHODS`를 연결해 두었고
  `every_method_is_routed` 시험이 지킵니다.
- **다중 노드 시험**: `tests/<영역>.rs`에서 `mod common;`으로 `Cluster`·`TestNode`·`befriend`·`eventually`·`next_notification`을
  씁니다. 노드는 127.0.0.1에서 직접 연결하고(mDNS·Tor·릴레이 끔), 고정 sleep 대신 조건을 기다립니다. 내부를 볼 때는
  `yangpa_daemon::testkit`(적용 결과, 저장된 이벤트, 키 그룹 사슬 끝·갱신 종류·표 보유 기기·키 지문(`crew_tip`·`key_update_kinds`·
  `key_table_digests`·`member_key_digest`), 뿌리 파생·중계(`can_derive_root`·`seal_relayed_roots`·`accept_relayed_roots`), 합성
  멤버(`synthetic_members`), 위조·변조 이벤트, 노드 간 요청 그대로 보내기, 적용 없이
  서명만 한 메시지·키 묶음, 시험용 채널 만들기, 크루 영역 전의 시험용 크루 `seed_crew`, 저장된 기기 목록, 확인 요청 유효 시간,
  시드를 훔친 기기 `install_stolen_seed`, 기기 연결 링크 끝내기 `expire_device_link`, 상한을 넘는 기기 목록, 전할 친구 삭제, 확인 요청 수). 설정을 바꾼 노드는 `cl.node_with("a", |c| c.push_max_devices = 0)`(gossip 경로), `|c| c.event_limit_devices = 0`(작은 크루에도 작성 기기별 상태 이벤트 제한).

## 이벤트 만들기 (`author`)

```
author_event(space, channel, body, key_choice, extra_parents) -> SignedEvent
  stream = (본문 레인, channel)
  parents = 그 스트림 머리(+ 같은 잠금 안에서 방금 만든 이벤트), 최대 16, ID 오름차순
  lamport = 공간 최댓값 + 1
  header.key = State(id) / Record{root} / Volatile{generation} / Sealed
    (상태 키 ID·뿌리 ID는 키에서 나옴: state_key_id, channel_root_id)
  본문 = 해당 키로 암호화(Sealed면 수신 기기 목록에 봉인). 평문 본문은 없음
  SignedEvent::sign(header, 본문, 내 기기 키)
publish_locked = author_event → ingest_locked(Source::Local) (받는 것과 같은 검증) → 거부면 오류
```

## 받은 이벤트 처리 (`ingest`)

모든 이벤트(로컬 작성 포함)는 적용 잠금 안에서 같은 경로를 탑니다. 출처는 `Source::{Local, Peer, Bootstrap, Replay}`.

1. 서명·본문 해시 검증, 공간 일치, 중복 제거(본문까지 있으면), 논리 시계 부풀림(공간 최댓값 + 2^20 초과) → RAM 보류.
   RAM 보류는 공간마다 4096개·대략 64MiB까지(넘으면 오래된 것부터 버림, 따라잡기로 다시 받음).
2. 채널 스트림: `KeyRef::Volatile`이거나 휘발성 채널이면 `volatile::on_inbound_event`로 넘기고 끝(디스크로 가지 않음).
   상태가 아직 없거나 채널을 모르면 RAM 보류. 폐기된 기기가 쓴 메시지는 글쓴이의 살아 있는 기기가 내줄 때만 받습니다(아니면 보류 없이 버림,
   protocol.md "폐기된 기기가 쓴 글"). 작성자가 지금 상태에서 멤버의 활성 기기가 아니면 **저장하지 않고 RAM 보류**
   (가입·기기 목록 갱신이 늦게 왔을 수 있음. 상태가 바뀌면 다시 보고, 새로 활성 기기가 된 피어와는 따라잡기를 돌림).
   저장소에서 다시 꺼낸 이벤트라면 거부. 공간 스트림 키 묶음도 상태가 있으면 같은 검사.
   짧은 서명(채널 스트림)은 1단계에서 모양·본문 해시만 보고 여기서 상태 기록의 키로 확인합니다(`devkeys::device_sign_keys`:
   기기 키 표 → 저장된 그 사용자의 서명 목록). 키를 모르면(체크포인트로 들어와 가입 이벤트의 키 목록을 못 본 기기) RAM 보류하고
   `IngestReport.missing_keys`로 잠금 밖에서 멤버 피어에게 `GetDeviceKeys`(`prefetch_device_keys`, 상태의 약속과 맞는 것만
   저장)를 묻고, 받았으면 보류를 다시 봅니다. 같은 기기는 30초 안에 다시 묻지 않습니다(`DeviceKeyTable::start_asking`).
   저장소에서 다시 꺼낸 것은 저장할 때 확인했으므로 키가 없어도 통과. 휘발성 채널 이벤트도 넘기기 전에 같은 확인을 합니다.
   부분 전달 스트림(받은편지함·키 그룹)은 해소기에 넣지 않고 `scoped_event`로: 내 받은편지함·지금 볼 수 있는 키 그룹(지금
   확정 기기 것)·내가 쓴 것·내가 전달자(`crews::is_inbox_relay`)인 사용자 앞으로 온 것만 다루고(나머지는
   `SyncState::foreign_scoped_bytes`에 세고 버림), 믿는 피어(`trusted_bootstrap`: 상태 전, 초대 묶음, 가입 중 받아 준 기기·공간을 만든
   사용자, 공간을 알려 준 내 기기)가 아니면 지금 상태로 먼저 거른 뒤 기준점 상태(`Resolver::state_at`)가 서면 `authorize_scoped`를 기준점 상태와 (믿는 피어가
   아니면) 지금 상태로 확인해 저장. 서기 전에는 `SpaceCore::scoped`(`scoped::ScopedQueue`, RAM, 따라잡기가 보는 ID는 잎 잠금 `ScopedSeen`, 공간마다 256개·16MiB·작성 기기마다
   3/4, 막던 이벤트가 풀릴 때만 다시 셈, 이벤트가 온 뒤 시작한 따라잡기 한 바퀴가 끝나거나(`ingest::note_caught_up`) 2분 뒤 빈 구간
   허용, 10분 뒤 버림). 믿는 피어가 보낸 내 받은편지함 묶음만 그 전에 열어 임시 키로 둡니다. 전달자로서 받은 것은 저장 뒤 그 사용자
   기기에 넘김(`IngestReport::relay`). 거부와 받을 까닭이 없는 것은 RAM에 기억해(4,096개) 따라잡기가 다시 받지 않음.
   protocol.md "부분 전달 스트림".
3. 레인별:
   - **State**: 상태 키로 복호화(없으면 저장하고 키 대기. 단 상태가 있는데 작성 사용자가 멤버가 아니면 `MemberJoin` 말고는
     저장하지 않고 RAM에 보류: 풀지 못한 상태 이벤트는 체크포인트를 막으므로, 쫓겨난 사람이나 새 신원이 아무 키 ID로 봉인한
     이벤트 하나로 크루 전체의 체크포인트를 멈추지 못하게) → 해소기(`Resolver`)에 넣음 → 상태가 바뀌면 저장·`space.updated`·
     `crews::on_state_applied`, 상태 전에 받은 첫 상태 키 묶음을 권한 확인 뒤 저장.
     상태 전에 받은 키 묶음(임시 키)은 그 상태 키를 기다리며 먼저 저장된 상태 이벤트를 바로 깨웁니다(새 기기가 시작 이벤트를
     키 묶음보다 먼저 받아도 상태를 세움).
   - **KeyAgreement**(`KeyUpdate`): 크루 갱신(공간 스트림)은 상태 이벤트처럼 갱신 전 크루 상태 키로 열어 해소기에 넣고, 조각을
     (작성 기기, 순번, salt)로 모아 둡니다(`keygroup::member::note_crew_part`). 해소기가 받아들인 갱신(`Resolved.key_events`, 마지막
     조각)은 해소 순서대로 합쳐(`validate_merged`) 적용합니다. 키 그룹 스트림의 갱신은 기준점 검사(`scoped`)를 지난 뒤 그룹마다
     `GroupChain`에 넣고 이긴 사슬을 적용합니다. 내 몫은 직접 상자·받은편지함 묶음·조각 블롭에서 찾고, 적용한 상태(T·R)는 순번마다
     최근 64개를 잠긴 키 저장소에 둡니다. 새 상태 키·뿌리를 얻으면 그 키를 기다리던 이벤트를 깨웁니다(`ingest::wake_needs`).
     계산한 T·R이 뿌리 약속과 다르면 위치만 증거로 남기고 경고합니다(키·내용은 남기지 않음). protocol.md "데몬 연결".
   - **Key**: 봉인·상태 키(크루 S, 키 그룹 스트림이면 그 그룹의 S_g)로 복호화 → 권한 확인
     (`authorize_message`) → **그 뒤에** 이벤트 저장, 상태 키·기록 키 저장 → 그 키를 기다리던 이벤트 재처리. 키를 기다리거나
     다른 기기 앞으로 봉인돼 열 수 없는 묶음은 그대로 저장, 열리지 않거나 권한이 없는 묶음은 저장하지 않습니다. 단, 내 앞으로
     봉인돼(`for_user`가 나) 열렸는데 지금 상태로는 작성자·대상을 모르는(멤버·기기·채널 오류, 권한 오류면 상태 키만 담았고 내가
     소유자·관리자일 때만: `may_settle_later`) 묶음은 저장하고 임시 묶음(RAM, 공간마다 256개)으로 두었다가 상태가 바뀔 때마다
     다시 봅니다(확정 기기 인계처럼 상태를 다 받기 전에 키가 먼저 오는 경우). 상태 키는 ID가 키에서 나온 것만 저장하고
     (`SharedKey::bound_to`), 작성자가 확정 기기·소유자·관리자가 아니라 인계 예외로만 통과한 묶음(`state_key_handover_only`)의
     상태 키는 저장만 하고 지금 상태 키로 올리지 않습니다(`keys::put_state_key(.., promote = false)`).
     크루 키 그룹 사슬 끝을 적용한 뒤에는 그 S가 지금 상태 키입니다(`keys::current_state_key`). 채널 뿌리도 ID가 뿌리에서
     나온 것만 저장하고(`SharedKey::bound_to`) 읽기에만 씁니다("보내는 쪽 규칙과 놓친 키" 참고). 그룹 R(`SharedKey::Root`, 가입
     묶음)은 멤버 상태에 넣어 그 R의 공개 뿌리를 파생하고, 기기 몫 묶음(`SharedKey::Bundle`)은 내 것만 두었다가 갱신의
     `bundle_root`로 확인해 적용합니다.
   - **Message**: 채널 뿌리의 기록 키(`record_key`)로 복호화(뿌리 저장소에 없으면 보관한 그룹 상태에서 파생해 보고, 그래도 없으면
     저장하고 키 대기 + `GetChannelRoots`) → 수정·삭제·반응·반응 지우기의 대상 확인: 같은 공간·같은 채널의
     `Message`·`ForumPost`여야 함(상태·키·키 합의 이벤트나 다른 채널 메시지면 거부). 대상을 모르면 대상과 무관한 권한만 본 뒤
     저장하고 대상이 올 때까지 대기(`MANAGE_MESSAGES`가 있어도) → `authorize_message` → 보기 행 생성·갱신 →
     `message.new`/`message.updated`(시작 때 다시 적용하면 알림 없음, 이 기기 사용자가 볼 수 없는 채널이면 알림 없음).
4. 키·대상·상태를 기다리는 이벤트는 디스크에 저장해 두고 대기 목록(설정 `space.wait.<공간>`)에 올렸다가 조건이 되면 다시 처리합니다.
5. 내가 만든 `KeyUpdate`는 해소기·그룹 사슬이 받아들인 것을 확인한 뒤에만 확정 기기 표를 바꿉니다(발행 실패·거부면 옛 표로 돌아감).
6. 잠금을 푼 뒤(`after_unlock`) 상태가 바뀌었으면 `keygroup::on_state_changed`, 새로 활성 기기가 된 인증된 피어와 따라잡기.

휘발성 이벤트는 저장 함수가 `PersistentEvent`만 받으므로 디스크로 갈 길이 없습니다(RAM 보류는 프로세스 메모리).

## 전파와 따라잡기 (`sync_service`)

- 전파: 공간의 활성 기기 `push_max_devices`(기본 8) 이하면 각 기기의 노드에 `PushEvents`(인증 후, 512개씩), 많으면 gossip
  `try_publish`. 받을 피어가 없으면(`NoPeers`) 직접 보내되, 받을 기기가 `push_max_devices`와 8 중 큰 수를 넘으면
  `NOPEERS_FANOUT`(8)곳만(`pick_targets`): 지금 인증된 연결이 있는 멤버 기기(`peers.authed_peers`, 이벤트 ID로 정한 랑데부
  해시 순서, `connected_targets`) 먼저, 모자라면 멤버 기기 목록에서 이벤트 ID로 정한 자리부터 골라 채움. 일부 멤버가 볼 수
  없는 채널(`VIEW_CHANNEL`이 없는 멤버가 있음)의 이벤트는 크루 토픽에 싣지 않습니다: 볼 수 있는 기기가 `group_topic_min`(32)
  이하면 모두에게 직접(`restricted_targets`), 많으면 그 키 그룹의 그룹 토픽(`topics.rs`, `publish_group`). 그룹 토픽에 받을
  피어가 없거나 보내지 못하면 8곳: 연결된 볼 수 있는 기기에서 4곳까지, 나머지는 볼 수 있는 기기 전체에서 이벤트마다 다른
  자리로(연결 없는 기기도 차례가 옴, 받으면 보낸 쪽에서 빠진 부모를 따라잡음). 키 그룹 스트림(`deliver_scoped`)도 그 그룹
  토픽이 있으면 같은 길입니다. 나머지는 따라잡기로 받습니다. 주소를 모르는 기기는 `locator::lookup`.
- 그룹 토픽(`topics::refresh`, 크루 토픽 허용 목록과 같은 때): 이 사본에서 볼 수 있는 기기가 `group_topic_min`보다 많은
  비공개 그룹 가운데 내가 볼 수 있는 것만 구독하고, 허용 목록은 그 그룹 채널을 볼 수 있는 멤버의 기기 노드
  (`member_peers_where`). 빠진 그룹은 구독과 목록을 풀고, 휴면·만료·공간 지우기 때 모두 풉니다(`topics::forget`). 그룹 토픽
  메시지는 그 그룹 채널·키 그룹 스트림 이벤트만 적용하고 나머지가 섞이면 `Ignore`(`gossip_verdict`).
- 이웃 고르기(`neighbors.rs`, `peers::spawn_redial` 주기): 토픽마다 연결된 멤버 기기가 `NEIGHBORS`(8)보다 적으면 노드
  기록(`identified_peers`, 7일 안에 성공)에서 골라 `peers::connect`, 소개받은 노드(`locator::intros`)에서 고르면
  `peers::reach`(인증해 그 기기일 때만). 크루 토픽이 모자라면 `locator::member_nodes_due`(다시 걸기 주기 10배)마다 연결된
  멤버 한 곳에 `locator::ask_member_nodes`. 걸어 본 기기는 `PeerDirectory::neighbor_dialed`로 간격을 두 배씩(최대 15분)
  미루고 인증되면(`set_authed`) 지웁니다. 연결이 `MAX_CONNECTIONS`(48) 이상이면 쉬고, 한 바퀴 4곳까지. 휴면·비활성
  공간은 건너뜁니다.
- 채널 토픽(`chantopics.rs`, 설정 `net.channel_topics`): 구독·허용 목록은 `refresh_space_peers`가 크루 토픽과 함께 맞추고
  (`chantopics::refresh`), 올릴 토픽은 전파가(`bucket_topic`·`notify_worthy`), 받은 것은 `gossip_verdict`가(`event_bucket`,
  알림이면 적용 뒤 `notify_worthy`·`notify_admit`) 봅니다. 연 채널 기록은 `channel.mark_read`에서(`follow`, 설정
  `chantopics.follow.<공간>`), 따라잡기 거르기는 `sync_filter`. 휴면이면 구독만(`forget`), 공간을 지우면 기록도
  (`forget_follows`) 지웁니다. 안 읽음 힌트는 `spawn_poll_tips`(다시 걸기 주기, 겹치지 않게)→`poll_tips`가 묻고 RAM의
  `ChannelTopics::tips`에 두며, `has_new`가 채널 보기를 채우고, 답은 `handle_get_tips`(`inbound.rs`)가 합니다.
- 채널을 누가 볼 수 있는지, 활성 기기 수와 목록, 기기 → 사용자 표는 상태 색인(`state_index::StateIndexes`,
  `SyncState::index`)이 상태 사본마다 한 번 계산합니다(3만 명이면 채널 하나에 5ms 남짓, 권한 규칙이 같은 채널끼리는 한 번).
  볼 수 있는 기기가 257대 이하면 그 목록을, 더 많으면 기기 목록 자리마다의 비트(4.5만 대에 6KB 남짓)를 담아, 보낼 곳 고르기가
  한 곳에 O(볼 수 있는 기기 사이 간격)입니다(4.5만 대를 매번 해시·정렬하지 않음). 색인은 계산한 `Arc<SpaceState>` 사본을 강한
  참조로 들고 묻는 쪽 사본과 포인터가 같을 때만 값을 주므로 권한을 막기 전의 '모두 볼 수 있음'이 새 사본에 쓰일 수 없습니다
  (상태는 바뀌면 새 `Arc`라 새 사본은 늘 새 색인). 기기 → 사용자 표(받기 쪽 멤버 기기 확인, 확정 기기의 그룹 맞추기)는 앞 사본의
  표에서 `SpaceState::changed_members`로 바뀐 멤버만 고칩니다(지속 맵이라 복제 O(1)). 공간을 지우면 색인을 버립니다.
  부분 전달 스트림도 같은 색인을 씁니다: 키 그룹 이벤트의 전달(`deliver_scoped`)은 그 그룹 채널 하나의 값(같은 그룹 채널은
  보기 규칙이 같아 보는 기기도 같음), 받은편지함은 상태의 그 사용자 기기 기록(`MemberDevices`)과 전달자 표. 내가 따라잡을 키
  그룹(`my_key_groups`)은 내가 볼 수 있는 채널의 `SpaceState::channel_key_group`(모든 멤버가 보는 채널은 크루 그룹이라 없음)이고,
  시더가 부분 전달 스트림을 내줄지(`may_read_scoped`)는 받기 쪽 확인처럼 지금 상태에서 바로 찾습니다. 넷 다 같은 그룹 규칙을
  씁니다.
- 봉인할 상자 키는 상태 기록의 키 약속으로 기기 키 표(`devkeys`)에서 찾습니다(LRU 4,096개, 없으면 DB `device_keys`를 묶어 한 번에,
  그래도 없으면 그 사용자의 저장된 서명 목록에서 같은 약속의 항목). 표는 상태 이벤트가 복호화된 직후(해소 전) 그 이벤트를
  저장하는 트랜잭션에 본문에 실린 목록의 키(`devkeys::body_keys`)를 그 공간 몫 행으로 함께 넣어 채웁니다(`ingest`의
  `store_state`). 서명은 보지 않습니다: 행은 키 약속으로만 찾고 읽을 때 약속을 다시 세므로 서명이 틀린 목록의 키는 상태가
  가리키지 않는 행이 될 뿐이고, 같은 기기 ID에 다른 키를 실은 목록도 다른 약속의 행이 되어 상태가 가리키는 키를 덮지 못합니다.
  시작 때 다시 적용(`Source::Replay`)하면 그 공간에 이미 있는 약속(`Store::device_key_commits`, 한 번 읽음)은 건너뜁니다.
  공간을 지우면 그 공간 몫 행만 지우므로 같은 기기가 있는 다른 공간은 계속 봉인할 수 있습니다.
- gossip 허용 목록(`refresh_space_peers`)은 상태가 바뀔 때, 시작 때, 친구 수락 때, 그리고 멤버 기기가 인증될 때마다 다시 세웁니다.
  고르는 일(`member_peers`)은 아는 노드만 훑습니다: 인증된 연결마다, 그리고 기기가 묶인 노드 기록(`Store::identified_peers`,
  쿼리 한 번)마다 그 기기가 지금 상태의 멤버 기기인지 봅니다(연결된 기기는 연결 쪽이 이김). 활성 기기마다 노드 기록을 찾으면
  4.5만 대 크루에서 상태가 바뀔 때마다(적용 잠금 안) 0.1초가 들었습니다.
- 받은 `PushEvents`: 내가 멤버인 공간에, 인증된 멤버 기기만(내 기기가 알려 준 상태 없는 공간은 `identity::is_adopting`).
  gossip: 전달한 피어가 허용 목록에 있어야 하고, 처음 올린 피어는 몰라도 됩니다(만나지 않은 멤버의 메시지가 중간 멤버를 거쳐 옴,
  `yangpa_net::gossip::GossipState::check`). 내용(작성 기기의 멤버십·서명·권한)은 적용 경로가 봅니다. 판정
  (`sync_service::gossip_verdict`)은 이 메시지의 이벤트만 보고, 상태와 상관없는 거부(`IngestReport::has_invalid`)면 `Reject`,
  지금 상태로 갈린 거부(`rejected_by_state`: 권한·멤버십·상태 규칙)면 `Ignore`, 새로 받은 것이 있으면 `Accept`.
  부모가 빠졌거나 보류가 생기면 그 피어와 빈 부모 채우기 따라잡기(`Pull::Gap`). 지금 상태로 거부한 경우에도 부모가 빠졌으면
  채웁니다: 가입 기록보다 먼저 온 크루 갱신처럼 빠진 부모 때문에 거부됐을 수 있고, 부모가 오면 해소기가 다시 해소해 받아들입니다
  (채우지 않으면 주기 따라잡기까지 멈춤).
- 작성 기기별 상태 이벤트 제한(활성 기기가 `NodeConfig::event_limit_devices`(기본 256)를 넘는 공간만): gossip·`PushEvents`와
  `Pull::Gap` 따라잡기는 `ingest::ingest_live`로 적용해, 이미 가진 것을 뺀 공간 스트림의 상태·키 합의 이벤트를 서명 확인 뒤(논리
  시계 보류보다 먼저) `ratelimit::EventLimiter`에 셉니다(버스트 30, 분당 10. 확정 기기·소유자 기기·관리 권한 멤버는 면제.
  보통 몫을 다 쓴 `MemberJoin`은 초대 증명을 풀어 확인되면 가입 몫 200. 멤버의 활성 기기가 아닌 작성 기기는 넘겨준 피어 몫).
  넘친 것은 `IngestReport::throttled`에 남기고 저장하지 않습니다: gossip은 `Ignore`, `PushEvents`는 `Ok`로 답하고 수만 디버그
  로그, `Pull::Gap`은 그 스트림을 멈춤(자식 이벤트 하나로 버린 부모를 모두 끌어오지 못하게). `Pull::Full` 따라잡기
  (`ingest::ingest`)는 묶지 않아 버린 것도 그때 묶음으로 들어옵니다.
- 따라잡기(인증 직후, 친구 수락 직후, 주기적으로 `sync_interval`는 `Pull::Full`, 받은 이벤트의 빈 부모 채우기는 `Pull::Gap`,
  같은 피어와 겹치면 끝난 뒤 넓은 쪽으로 한 번 더): 공간 스트림 먼저, 그다음 일반 채널마다
  `GetHeads` → 모르는 머리부터 `GetEvents`로 부모를 따라감(최대 8단계) → 그래도 다르면 지문 비교(`yangpa_sync::reconcile`).
  `Pull::Full`로 받은 묶음은 바로 적용하지 않고 모았다가(`PullBuffer`) 묶음 4개, 2초, 모은 크기 약 8MB(본문 + 서명·머리
  어림), 또는 빠진 부모가 없을 때(받은 갈래가 다 이어져 해소가 끝까지 나아감) 한 번에 `ingest`합니다. 적용마다 해소·바뀐 상태 행
  쓰기·색인 다시 셈·알림이 들어, 큰 크루를 따라잡을 때 그 횟수를 1/4로 줄입니다(결과는 한 `ingest`에 더 넣은 것과 같음). 모아
  둔 이벤트는 저장 전이므로 더 받을 것을 고를 때 아는 것으로 셉니다. 받기가 도중에 실패하면 모아 둔 것을 먼저 적용하고 오류를
  돌려줍니다(회로가 자주 끊기는 피어에게서도 나아가게). `Pull::Gap`은 작성 기기별 제한을 묶음마다 확인해야 하므로 모으지 않습니다.
- 시더 응답: `GetHeads`·`GetFingerprints`·`GetIds`는 내용 없는 검사, `GetEvents`는 요청에 적은 공간의 이벤트만 골라
  이벤트마다 `yangpa_sync::policy::check_seed_request`(멤버 → 채널 권한 → 입장 이전 기록 공개 → 제공 범위 → 대역폭).
  입장 시점은 해소 상태의 `member_since`, 모르면 가장 엄격하게.

## 피어와 인증 (`peers`)

- 연결되면, 저장소가 신원을 아는 노드에게 먼저 보내지 못한 연락처 수락(`FriendAccepted`)을 보내고 `Auth`를 주고받은 뒤
  따라잡습니다.
- 받는 쪽은 그 기기가 내 기기, 내가 멤버인 공간(비활성화된 1:1 공간은 빼고)의 멤버 활성 기기, 친구·임시 연락처(또는 내가 연락처
  링크를 쓴 사람, `friends::has_outgoing_request`)의 활성 기기일 때만 자기 `Auth`로 답합니다. 만료된 연락처면 `Expired`로 거절하고, 그
  답을 받은 쪽은 자기 임시 연락처도 만료합니다(`friends::on_expired_by_peer`). 기기 판정은 `identity::auth_status`로, 내가 아는 그 사용자의 목록(내 기기면 내 목록,
  처음 보는 사용자면 제시된 목록)에 비춥니다:
  - `Active`: 아는 목록에서 활성이거나, 제시된 목록이 아는 목록을 이어받고 아는 목록의 활성 기기가 공동 서명함.
  - `Revoked`: 아는 목록에서 폐기됨. 그 목록을 실은 `YpResponse::Revoked`로 답해 그 기기가 스스로 지우게 합니다.
  - `Unknown`: 아는 목록에 없고 활성 기기의 공동 서명도 없음(시드만 가진 기기, 옛 빌드의 복구 기기). 함께 있는 공간 상태가 그
    기기를 담았어도 믿지 않고 `NotMember`로 거절합니다.
  시각 오차 5분.
- `Revoked` 답을 받은 쪽: 그 목록을 내 목록으로 받아 보고(내 활성 기기가 공동 서명한 판이거나, 답한 노드가 다른 사용자의 기기이고
  판이 내 목록을 이어받으며 공동 서명 기기를 내가 모를 때: `identity::on_revoked`) 이 기기의 데이터를 지웁니다. 지운 뒤에는 데몬이 다시 떠 새 PeerId와 onion을 씁니다(키를 잃은 onion
  서비스는 새 연결을 받지 못해, 그대로 새 계정을 만들면 이미 연결된 노드만 닿습니다).
- 인증을 건 쪽은 인증 뒤 `identity::on_authenticated`로 프로필을 교환하고, 받은 쪽은 `identity::on_auth_accepted`로 전하지 못한
  친구 삭제를 전합니다. 내 `Auth`가
  `NotMember`로 거절되면 그 사용자에게 전할 친구 삭제를 지웁니다(상대가 이미 반영함).
- `handle_auth`가 `NotMember`로 거절한 연결은 그 기기와 함께 기억하고(`PeerDirectory::note_refused`, 32개까지, 끊기거나
  인증되면 지움), 상태가 바뀌어(`crews::on_state_applied` → `peers::on_state_applied`) 그 기기를 멤버로 알게 되면 이쪽에서
  `ensure_auth`·따라잡기를 합니다. 거절과 기록 사이에 상태가 먼저 따라잡혔으면 기록 직후 다시 보고 바로 겁니다.
- 인증 전에도 받는 요청: `Auth`, `RedeemFriendLink`, `FriendAccepted`, `RedeemInvite`, `Ping`, `DeviceLink`·`DeviceLinkPoll`(링크
  토큰을 가진 쪽만, 답은 요청한 기기의 상자 키로 봉인)(`inbound.rs`).
  `P2pHello`는 Tor로 인증된 피어에게서만 받습니다. P2P 스웜에서 들어온 요청은 따로 거릅니다("P2P 전용 채널" 절).
- 연결에 성공한 주소는 `peer_cache`에 남기고 재시작 때 최근 64개 노드에 다시 연결합니다. 그중 친구·내 기기 노드(`close_contact`)는
  인증까지 기다리는 동안 그 사용자를 `net.status`의 `connecting`에 두고, 하나 끝날 때마다 `net.status`를 알립니다(앱이 '연결 중'으로 보임).
- 링크용 주소 교환: 인증을 건 쪽이 `on_authenticated`에서 `peers::exchange_contact`(`Social(Contact)`)로 내 주소를 보내고 답으로
  상대 주소를 받습니다. 받은 쪽은 `identity::handle_social`에서 `peers::record_contact`. 둘 다 `contact_addrs_from`으로 거른 뒤
  (릴레이 경유는 회로 뒤에 그 피어 `/p2p`만, onion은 하나짜리만, 8개) 새 주소 먼저 16개까지 `record_peer_success`로 남깁니다.
  `NetEvent::Status`마다 `peers::on_status`가 내 링크용 주소(`NetStatus::shareable_addrs`: Tor 스웜은 onion, 직접 연결 스웜은
  릴레이 경유. PeerId와 함께)를 지난번과 비교해 바뀌었으면 인증된 피어 모두와 다시 교환합니다(보관 전용 연결은 빼고). 상대가
  친구·내 기기(`close_contact`, 임시 연락처는 아님)면 `tor::node_contacts`(지금 노드 하나: PeerId와 onion)도 싣고, 받은 것은 4개까지
  `record_node_contacts`로 남깁니다(지금 노드가 아니면 `peer.prev.<PeerId>`, onion 주소는 그 노드 기록에. 예전 빌드에서 PeerId를
  바꾼 기기를 다시 찾는 데 씀). 연락처 수락을 받은 쪽(`friends::handle_accepted`)은 인증 뒤 한 번 더
  교환하고, 임시 연락처가 친구가 되면(`friends::become_friend`) 인증된 그 사람 기기와 다시 교환합니다(처음 연결 때는 아직 친구가
  아님).
- 다시 걸기(`peers::spawn_redial`, `redial_interval` 기본 60초): 휴면이 아니고 비활성화되지 않은 1:1 공간의 상대 기기, 내 다른
  기기, 작은 크루(활성 기기가 `push_max_devices` 이하)의 멤버 기기 중 인증된 연결이 없는 기기를 한 번에 4개까지(보관 전용 상대는 빼고). 기기마다 다음 시각과 간격(두 배씩 최대 15분, Tor
  스웜이면 시작을 두 배)을 두고 인증되면 지웁니다. 기기에 묶인 노드 → `peer.prev.<PeerId>`로 기억한 그 기기의 예전
  노드 → `locator::lookup`(위치 기록, 없으면 멤버 소개) 순. 이미 연결된 노드면 연결 이벤트가 다시 오지 않으므로 직접 `ensure_auth`.
- `remember`는 기기가 다른 PeerId로 인증하면 저장소에서 떼어지는 예전 노드를 `peer.prev.<예전 PeerId>`({user, device})로
  남기고, 지금 노드의 표시는 지웁니다. `on_connected`는 이 표시가 있는 노드도 아는 노드로 보고 인증합니다.

## 연락처와 친구 (`friends`)

연락처 링크를 쓰면 사람의 수락 없이 둘이 **임시 연락처**가 되고, 둘 다 `friend.propose`로 친구 추가를 요청하면 친구가 됩니다.
친구 기록의 `kind`는 `Friend`·`Temporary`·`Expired`(protocol.md "연락처 링크와 친구").

1. `friend.link.create`: 토큰 해시만 저장, 링크에는 이 기기의 onion(`tor::device_onion`, Tor 부트스트랩 전에도 키에서 나옴)과
   릴레이 경유 주소(직접 연결 스웜만)·PeerId만(자기 IP 없음).
2. `friend.add`: 내 링크는 거절합니다. 이미 친구이거나 이 링크로 이미 임시 연락처면 `connected`. 아니면 보낸 요청과 보낼 대기열
   (`friend.pending_redeem.<요청>`: 링크의 연락처·토큰·만료·시도 수·다음 시각)을 먼저 저장한 뒤 첫
   시도(30초, Tor로 도는 기기는 120초): 링크 주인 노드에 닿게 하고(이미 연결 → 링크의 onion·릴레이 경유·전에 연결했던 주소 →
   DHT, 그래도 닿지 않으면 PeerId 없이 onion으로 걸어 답한 노드: 예전 빌드에서 PeerId를 바꾼 링크 주인) `RedeemFriendLink`(인증은
   보낼 때마다 새로 서명). 상대가 거절하면 대기열과 요청을 지우고 오류, **닿지 않거나 응답을 받지 못하면 `pending`**(요청은 닿았을
   수 있으므로 보낸 요청은 남김). 대기열은 그 노드와 연결될 때(`peers::on_connected`, 신원을 모르는 노드라도)와 재연결 주기
   (`friends::retry_pending_redeems`, `redial_interval`에서 두 배씩 최대 30분)에 다시 보냅니다. 보내는 중인 요청은
   `Node::friend_sending`에 표시해(첫 시도는 대기열에 넣기 전에) 겹쳐 보내지 않고, 보내기 전에 다음 시각을 미뤄 둡니다. 다시 보낸
   요청이 거절되면 대기열만 지웁니다(수락 중의 답일 수 있어 보낸 요청은 남김). 취소(보낸 요청이 없음)·이미 친구·링크 만료·7일이
   지나면 대기열을 지웁니다.
3. 받는 쪽(`handle_redeem`): 기기 증명·프로필 서명을 확인합니다. 이미 친구면 `BadRequest`("이미 친구"), 이 링크로 이미
   임시 연락처이거나 받아들이는 중이면(`Node::contact_accepting`) `FriendPending`, 차단한 사용자면 `moderation::silent_friend_refusal`.
   처음인 사람이고 임시 연락처가 256개(`MAX_TEMPORARY_CONTACTS`)면 `BadRequest`("상대의 임시 연락처가 가득 찼습니다"). 토큰(만료
   `Expired`·1회용·폐기 `InvalidToken`)을 쓰고 요청을 남긴 뒤 `FriendPending`으로 답하고, **사람의 수락 없이** 바로
   `accept_contact`로 받습니다. 같은 사람이 같은 링크로 다시 보내면 링크 사용 횟수를 다시 쓰지 않고, 이미 받았는데 아직 전하지
   못한 수락이면 그것을 다시 보냅니다.
4. `accept_contact`: 요청을 먼저 가져가고(겹쳐 받아도 공간 하나, 실패하면 요청을 되돌려 상대가 다시 보낼 때 다시 받음)
   - 처음이면 `accept_claimed`: 1:1 공간 생성(첫 상태 키 봉인 → `SpaceCreate` → 첫 채널 "일반" → 첫 크루 키 갱신(두 사람
     기기 모두, 직접 인코딩), 중간에 실패하면 만들던 공간을 지움) → 임시 연락처 기록(`expires_at`은 링크의 만료, `link_hash`).
   - 기록이 있고 그 1:1 공간에 아직 둘 다 멤버이면(만료된 연락처가 새 링크를 씀) `reactivate`: 같은 공간으로 다시 임시 연락처.
   그다음 보낼 수락을 저장하고 `FriendAccepted{token, space, 공간 스트림 이벤트(최대 512개), 내 서명된 프로필,
   contact: ContactTerms{expires_at}}` 전송. 전하지 못하면 그 노드와 다시 연결될 때마다 보냅니다. 같은 수락은 한 번에 하나만
   보냅니다(`AcceptSending`: 처음 보내기·연결 때·요청 재수신이 겹치면 건너뛰고 끝난 뒤 다시 봄). 상대가 `InvalidToken`·
   `BadRequest`로 답하면(요청을 모름·취소함) 기록과 공간을 되돌립니다. 기록은 `contact_guard` 안에서 아직 그 공간을 가리킬 때만
   지웁니다(그사이 받은 수락이 기록을 다른 1:1 공간으로 옮겼으면 공간만 지움).
5. 요청한 쪽(`handle_accepted`, 인증 전에도 받음): 자기가 보낸 요청의 토큰인지 본 뒤, **아무것도 저장하기 전에** 묶음을
   검사합니다 — 공간 스트림 이벤트만, 512개 이하, 링크 주인이 작성한 `SpaceCreate`가 정확히 하나, 그것을 나에게 봉인된 키 묶음의
   상태 키로 열어 버리는 해소기에 넣었을 때 링크 주인이 소유한 나와 둘만의 1:1 공간(서명된 기기 목록 확인 포함). 통과하면 그 ID로
   시작 이벤트를 고정 → 이벤트 적용(첫 크루 키 갱신의 직접 상자로 크루 키를 얻음) → 같은 조건을 다시 확인(실패하면 이번에
   만든 공간을 지움) → `contact`가 있으면 임시 연락처, 없으면(예전 빌드의 수락) 친구로 기록 → 인증, 따라잡기, 주소 교환. 있던
   공간인지 보기부터 조건 확인까지 그 공간 잠금(`lock_space`)을 쥐고, 쥔 뒤 요청이 아직 있는지 다시 봅니다. 요청이
   없으면(`handled_resend`) 처리한 수락의 재전송으로 `Ok`: 그 공간이 연락처 기록의 공간이거나, 기록이 그 사람과의 ID가 더 작은
   다른 1:1 공간을 가리켜 겹친 공간으로 지운 것(묶음 검사를 다시 통과해야 함, 인증 전이라 연락처 여부를 떠보지 못하게). 그 밖은
   `InvalidToken`.
6. 서로 상대 링크를 동시에 쓰면 양쪽이 각자 1:1 공간을 만들고 상대 것도 받습니다. 둘 다 ID가 작은 쪽을 남기고 다른 하나를
   지웁니다(`duplicate_space`. 내가 만들어 아직 수락을 전하지 못한 공간이면 전한 뒤에 지움). 기록이 정해지면 `clear_requests_with`가
   그 사람과의 남은 요청과 대기열을 지웁니다. 수락한 쪽은 받은 요청만 지우고 보낸 요청은 남깁니다: 상대도 연결이 끊긴 사이나
   동시에 내 요청을 받았으면 그 `FriendAccepted`는 보낸 요청의 토큰으로만 맞춰지고, 못 맞추면 양쪽이 서로 "모르는 요청"으로
   거절해 둘 다 되돌립니다. 남은 보낸 요청은 연락처를 지울 때(`remove`, 상대가 나감) 지웁니다.
7. 만료(`spawn_contact_expiry`, 5초마다 `expire_due`): `expires_at`이 지난 임시 연락처를 `Expired`로 바꾸고 `on_contact_expired`
   — 친구 추가 요청을 지우고, 인증된 그 사람 기기(아직 내 쪽 인증이 끝나지 않은 1:1 공간 기기 포함)에 `Social(ContactExpired)`로
   알린 뒤, 다른 공간을 함께 쓰지 않으면 인증을 풀고(`peers::drop_untrusted_user`) `space.updated`·`friend.updated`. 링크를
   폐기하면(`friend.link.revoke`) 그 링크의 임시 연락처를 바로 만료합니다. 상대는 다음 인증이 `Expired`로 거절될 때 자기 쪽도
   만료합니다. `ContactExpired`를 받으면 바로 만료하지 않고 인증을 다시 보내(`peers::reconfirm_auth`) 그 답으로 정합니다(알림을 늦게
   처리하면 그사이 새 링크로 되살린 연락처를 만료하므로). 그 인증이 닿기 전에 상대가 꺼지면 상대가 다시 켜진 뒤의 인증에서
   만료합니다. 인증을 보낸 뒤 기록이 바뀌었으면 `Expired` 답도 두지 않습니다. 만료된 연락처와의 1:1 공간은 비활성화됩니다(`space_deactivated`: 쓰기는 `1003`, 전파·따라잡기·
   시더 응답·다시 걸기·신뢰 근거에서 빠지고 지난 대화만 읽음). 만료된 사람이 새 링크를 쓰면 4의 `reactivate`로 되살아납니다.
8. 친구 추가(`friend.propose` → `propose`): 친구면 바로 `friends`, 만료된 연락처면 `1005`, 연락처가 아니면 `1004`. 보낸 요청
   (토큰 해시 자리가 0인 요청)을 남기고 인증된 그 사람 기기에 `Social(Propose)`. 받은 쪽(`handle_propose`)은 만료된 연락처면
   `Expired`, 연락처가 아니면 `NotMember`, 이미 친구거나 자기도 요청했으면 친구로 올리고 `SocialReply.friends = true`, 아니면 받은
   요청으로 남기고 `friend.request` 알림(`friend.accept`는 같은 요청을 보내는 것과 같고 `friend.reject`는 지움). 상대가 먼저
   요청해 두었으면 이쪽은 바로 친구가 되고 상대에게도 보냅니다. 닿지 않은 요청은 다음 인증 때(`friends::on_peer_authed`) 다시
   보냅니다. 친구가 되면(`become_friend`) 링크 흔적과 요청을 지우고 `friend.updated`, 인증된 기기와 주소를 다시 교환합니다(이제
   노드 기록도 실림).

## P2P 전용 채널과 신뢰 스위치 (`p2p`, `trust`)

프로토콜은 protocol.md "P2P 전용 채널"·"신뢰 스위치와 직접 연결". 데몬 쪽 상태는 `P2pState`·`TrustState`이고 참여 기록과 신뢰
스위치 말고는 모두 RAM에만 둡니다(상대 P2P 주소를 디스크에 남기지 않음).

- **내 기기끼리**: `trust::trusts`는 받아들여진 기기에서 나 자신에게 참이고, `trust::own_devices`(내 다른 활성 기기가 있음)가 직접 스웜을
  띄웁니다. `trust::any`는 그대로라 통화의 STUN 내주기는 바뀌지 않습니다. 내 목록이 바뀌면(`adopt_own_list`·`revoke_device`) `p2p::reconcile`을 부릅니다.

- **참여 기록**: 설정 `p2p.joined`(공간·채널 ID 쌍, 4096개까지, 읽은 값은 캐시). `channel.p2p_join`(`p2p::join`)은 멤버이고 그
  채널을 볼 수 있으며 채널이 지금 P2P 전용일 때만 넣고, `p2p_leave`는 늘 됩니다. `p2p::on_state_applied`가 P2P 전용이 아니게 된
  채널과 내가 나간 공간의 기록을 지웁니다. `channel.set_p2p`·`channel.create`의 `p2p`는 `p2p::check_set`·`check_create`로 해소기
  규칙을 먼저 보고 `ChannelTransportSet`을 냅니다(만들 때는 `ChannelCreate`와 한 적용 잠금 안에서).
- **신뢰 스위치**(`trust.rs`): 설정 `trust.users`·`trust.spaces`(각 4096개까지, 쓰기 잠금 하나로 읽고 고침). `set_user`는 나
  자신을 거절하고, 켤 때는 함께 멤버인 활성 공간이 있어야 합니다. `set_space`는 내가 멤버인 크루만, 바뀌었는지 돌려줘
  `space.updated`를 그때만 냅니다. `trusts`는 사람 스위치이거나 나와 상대가 함께 멤버인 신뢰한 크루가 있는지, `via_crews`는 그
  크루 목록(나 자신이면 빈 목록), `any`는 켠 것이 하나라도 있는지입니다. 바꾸면 `p2p::reconcile`.
- **스웜 둘 띄우고 내리기**(`reconcile`, `life` 잠금으로 하나씩):
  - 직접 스웜: 쓰는 참여 채널(`active_joined`: P2P 전용, 내가 멤버, `VIEW_CHANNEL`, 비활성화된 1:1 공간 아님)이 있거나
    `trust::any`이면. `NetMode::Normal`에 새 `Keypair::generate_ed25519()`, 데이터 경로 `<data>/p2p`, mDNS 끔, Kademlia
    클라이언트. 수신 주소는 `NetOptions.p2p_listen`(`--p2p-listen`, 시험)이 있으면 그것이고, 없으면 UPnP를 켜고 `probe`가 고른
    UDP 포트에서 STUN을 물은 뒤 그 포트로 QUIC(+ 아무 TCP 포트)을 엽니다. 답한 공인 주소가 서버마다 같고 공인 주소일 때만
    `external_addrs`에 넣습니다. 그새 포트를 뺏겼으면 아무 포트로 열고 공인 주소는 비웁니다.
  - 출구 스웜: 나를 신뢰한 기기 중 내가 신뢰하지 않아 출구로 걸 기기가 있으면(`wants_exit`). 주 스웜의 `NetHandle::tor_share`가
    있으면 `NetMode::Tor`에 `onion_listen = false`, 그 Arti를 함께 씁니다. 없으면(`--direct` 주 스웜) 받지 않는 `NetMode::Normal`.
  - 이벤트 처리 작업은 스웜이 바뀌면 끝나고, 들어온 gossip은 `Ignore`, 연결되면 알게 된 기기의 노드에만 `ensure_auth`, 상태
    알림마다 `announce`. 내릴 때는 그 스웜의 P2P 연결마다 끊김 처리(`volatile::on_peer_disconnected`)를 하고 P2P 인증을 지웁니다.
- **안내**(`announce` → `send_hello`): Tor로 인증된 피어마다 `hello_for`로 맞춘 `P2pHello`를 만들어 `sent`의 지난 것(`seq`
  빼고)과 다를 때만 `stamp`로 `seq`를 붙여 보냅니다. 받은 답과 들어온 안내(`handle_hello`)는 `learn`: 그 기기에서 받아들인
  것보다 작은 `seq`면 버리고(`seen`), 자격이 맞는 채널만 남기며, 직접·출구 노드가 모두 없으면 그 기기를 잊습니다. 새 출구 노드를
  알게 되면 그 노드로 `ensure_auth`를 걸어 봅니다. 상대가 함께 참여함을 알게 되면 다시 `announce`해 이번에는 주소를 싣습니다.
  예전 빌드가 다른 답을 하면 전해진 것으로 봅니다. `on_peer_authed`는 그 피어의 `sent`·`seen`을 지우고 다시 알립니다.
- **걸기**(`connect_all`, `want_for`가 정함): `Direct`는 직접 스웜에서 상대 직접 노드의 주소 모두로, `ExitOut`은 출구 스웜에서
  상대 직접 노드의 공인 TCP 주소(`exit_dialable`)로 겁니다. 같은 (기기, 쪽, 상대 노드)는 `REDIAL_GAP`(5분)에 한 번이고, 걸린 뒤
  `ensure_auth`를 0.5초 간격으로 3번까지 합니다. 직접 걸기가 실패하고 기기 ID가 작은 쪽이면 `punch`: 연결이 없고 STUN이 25초보다
  오래됐으면 `fresh_node`로 직접 스웜을 다시 띄워 새 포트·공인 주소를 얻고, Tor로 `P2pPunch`를 보내 상대 답(상대의 새 노드)을
  배운 뒤 다시 겁니다. 받은 쪽(`handle_punch`)은 답을 보낸 뒤 상대 QUIC 주소로 `dial_as_listener`를 5초씩 3번까지 겁니다.
- **P2P 인증**(`ensure_auth`·`handle_auth`): 피어마다 잠금을 두고, `trusted`(알게 된 그 기기의 직접·출구 PeerId, 서명·시각, 활성
  기기)와 `link_ok`(쪽마다의 조건)를 확인한 뒤 `PeerDirectory.p2p_authed`에 넣고 그 피어가 어느 스웜에 있는지(`sides`) 기억합니다.
  인증되면 함께 참여한 공간을 따라잡고, 일반 내용을 나르는 연결이면 그 기기의 Tor 피어로 공간 따라잡기를 한 번 더 합니다(요청이
  빠른 연결로 감). `p2p_authed`는 신원 확인(`identity_of`)에만 쓰고 기기의 노드·접속 상태·주소 기록에는 쓰지 않습니다(Tor로 오갈
  것이 P2P로 새지 않게). `prune`(상태·신뢰가 바뀐 뒤)은 `link_ok`가 거짓이 된 P2P 인증을 지웁니다.
- **받는 요청**(`handle_inbound`): 인증 전 `Auth`·`Ping`, 인증 뒤 동기화(`PushEvents`·`Get*`)·블롭·`VolatileSnapshot`·
  `VolatilePresence`·`VolatileKeyWrap`·`Typing`만 각 처리기로 넘기고 나머지는 `Forbidden`. 처리기 안의 `p2p::route_ok`·`accepts`가 채널마다 다시 거르고,
  P2P 전용이 아닌 내용은 `carries_normal`(둘 중 하나라도 신뢰)인 연결에서만 받습니다.
- **길 나누기**: 키는 공간·키 그룹 스트림의 `KeyUpdate`와 받은편지함의 가입 묶음으로 Tor로 오므로 P2P 전용 채널에 따로 낼
  키 묶음이 없습니다(채널 스트림이 아니라 참여 전에도 키를 받음). 고리는 위 표
  (`may_author`·`accepts`·`route_ok`·`split`·`push`·`shared_channels`·`channel_peers`·`request`·`p2p_joined`).
- **통화**: `voice::voice_block`은 P2P 전용 채널이면 참여했는지만 보고(아니면 `1003`), 나머지 채널은 Tor로 도는 기기면 Tor 통화
  (`voice::tor_call`, `voice::media`: WebRTC 없이 봉인한 소리 패킷을 `/yangpa/media/1`로)입니다. 신뢰 스위치는 통화 길을 바꾸지 않습니다. Tor 통화에서 켜 둔 신뢰가 없거나 익명 채널이면 `voice.join`의 `ice_servers`는 비고, 아니면 `stun::call_ice`입니다.
  `reconcile` 끝의 `voice::leave_blocked`가 막힌 통화에서 나옵니다.
- **로그**: 주소·IP는 남기지 않습니다(PeerId·쪽·오류 종류만).

## 커밋 확정 노드 (`keygroup`)

내 기기가 `state.committer`이면 상태가 바뀔 때마다(`keygroup::on_state_changed`) 바퀴를 요청합니다. 바퀴는 묶음 창
(`commit_batch_window`, 기본 300ms)만큼 기다렸다 돌고, 그 사이 빚(뺄 기기)이 또 늘면 `commit_batch_max`(2초)까지 한 창씩 늘려
대량 추방을 갱신 하나로 묶습니다. 추방·밴·기기 폐기도 창을 열고 늘릴 뿐이고, 창을 건너뛰는 것은 제거 반영을 기다리는 보내는
쪽(내가 확정 기기일 때)뿐입니다(`keygroup::urgent`).
도는 중에 온 요청은 끝난 뒤 한 번 더 돕니다. 초대를 받아 준 확정 기기는 응답하기 전에 자기 요청이 반영될 때까지 기다립니다
(`reconcile_batched`: 동시에 온 가입자들이 같은 `Advance`로 들어감). 바퀴 하나(`keygroup::committer::round_locked`, 그 공간 적용
잠금 안, protocol.md "데몬 연결"):

0. 공간 상태(`Arc`)·크루·그룹 사슬 끝·할 일 지문이 앞 바퀴와 같고 앞 바퀴가 다 정리됐으면 건너뜁니다(`RoundMemo`, 메모리에만).
1. 표(`committer::Table`: 그룹마다 `Arc<KeyTree>`·`Arc<CoverGroup>`, 노드 캐시. 저장은 아래 "표 저장 칸")를 해소기의 크루 사슬
   끝·그룹 사슬 끝과 맞춰 봅니다. 맞지 않거나 표가 없으면(넘겨받지 못함) `key_handover_wait`(20초)를 기다린 뒤 공개 끝으로 이어받고
   (`KeyTree::recover`) 크루 전체 재키부터 합니다. 그룹 사슬 끝은 쓸 수 있는 끝이되, 넘겨받은 표의 그룹 상태가 그보다 앞서고
   이긴 사슬에 있으면(옛 확정 기기가 낸, 새 확정 기기의 갱신이 붙기 전까지는 쓸 수 없는 갱신) 그 갱신을 끝으로 봅니다
   (`GroupChain::winning_at`). 이긴 사슬에도 없으면(비공개 그룹 밖에서 확정 기기가 된 관리자라 그 그룹 스트림을 아직 못 받음)
   그 그룹만 건너뛰고 멤버 3곳과 따라잡은 뒤 2초 뒤 다시 돌며, `key_handover_wait`가 지나도 못 받으면 사슬 끝에서
   다시 시작합니다(`KeyGroupState::lag_since`). 넘겨받은 쪽도 넘겨준 기기와 곧바로 따라잡습니다.
2. 크루: R 새로 하기가 남았고(`Meta::pending_refresh`: 채널이 크루를 떠남·기록 공개를 끔, 또는 24시간·가입 1,000대) 이번 갱신이
   전체 재키도 제거도 아니면 `RRefresh`부터 따로 냅니다(가입 전진이 옛 R을 넘기지 않게). 그다음 빚을 빼고 새 활성 기기를
   넣습니다(제거와 가입이 같은 `Rotate`). 넣는 기기는 그 기기를 활성으로 만든 이벤트가 갱신
   머리 조각의 부모에서 닿아야 해서, 공간 스트림 머리가 부모 상한(16개)을 넘으면 아직 닿지 않는 가입을 가장 많이 덮는 머리부터
   부모로 고르고(`committer::head_parents`, `Resolver::joins_reachable`) 덮지 못한 가입은 다음 바퀴에 넣습니다. 해소기가 요구하거나(`rekey_required`), 표를 잃었거나,
   일주일(`FULL_REKEY_EVERY`, 마지막 전체 재키 시각은 공개 메타데이터 `kg.meta.<공간>`에 두고 표를 넘길 때 함께 보냄)이
   지났으면 `FullRekey`(트리면 기기 몫이 64묶음을 넘을 때 사용자별 조각 블롭, `keygroup::shards`).
3. 비공개·덮개 그룹(`desired_groups`): 멤버 전원이 보는 채널은 크루를 쓰고, 나머지는 열람 규칙마다 그룹 하나. R 새로 하기가
   남은 그룹은 `RRefresh`만 내고 그 그룹의 가입은 다음 바퀴에(같은 까닭). 공개 끝의 `fresh_crew`가 크루 전체 재키보다 뒤처진
   그룹은 새 T(`mark_stale`). 내기 전에 `GroupChain::would_accept`로 미리 판정해 거부될 갱신은 내지 않고 1초부터 30초까지 늘려
   가며 다시 돕니다(`KeyGroupState::group_retry`). 그룹 갱신은 크루 갱신 뒤의 크루 상태 키로 암호화하고 기준점에 크루 사슬 끝을
   마무리한 조각(`tip_part`)을 넣어 그 그룹의 키 그룹 스트림에 냅니다.
4. 가입한 사용자마다 받은편지함 키 묶음(`share_bundles`): 상태 키(`keys::keys_for_joiner`: 바탕 체크포인트가 있으면
   `valid_state_keys`·체크포인트 이벤트 키와 크루 머리 뒤 키 가운데 최근 `keys::JOINER_STATE_KEYS` 64개, 없으면 최근
   64개. 지금 키를 마지막에, 나머지는 가입자가 `GetStateKeys`로 물음), 크루 R, 트리 가입 기기의 몫 묶음. 512개·약 192KiB씩 나눠 순서대로 냅니다(`keys::chunk_shares`). 전파는 `sync_service::deliver_scoped`: 그 사용자의
   활성 기기, 보관 노드, 그 사용자의 전달자(`crews::inbox_relays`)에만 직접. 그때 꺼져 있던 전달자는 확정 기기와 따라잡을 때 당겨
   갑니다(`crews::relayed_users`).
5. 기록 공개를 켠 채널은 비공개 시절 뿌리(뿌리 저장소에 있는 것)를 그 그룹의 지금 상태 키로 채널 스트림에 다시 봉인합니다
   (`reseal_roots`, `SharedKey::ChannelRoot`).

갱신은 표의 복제본에 하고, 해소기(크루)·그룹 사슬이 받아들인 것을 확인한 뒤에만 표를 바꿉니다(발행 실패·거부면 옛 표로
돌아가고 다음 바퀴에 다시).

표 저장 칸: 그룹마다 칸 두 개. 내기 전에 다음 상태를 지금 칸이 아닌 칸에 써 두고(`stage`), 받아들여지면 그 칸을 지금 칸으로
삼습니다(`Table::settle`). 바퀴 끝(`flush`)에는 써 두지 못한 그룹·목록만 쓰고 빠진 그룹의 칸을 지웁니다. 저장 오류는 표를
되돌리지 않습니다(메모리 표가 기준). 캐시가 없을 때(`load_stored`)는 그룹마다 사슬에 있는 칸, 없으면 순번이 큰 칸을 고릅니다.
크루 트리 잎은 저장·넘기기에서 빼고 leaf_master로 다시 만듭니다(`yangpa_crypto::keytree` 스냅샷 3판). 확정 노드가 오프라인이면 다른 노드는 아무것도 하지 않고, `space.get`의 `pending_commit`이 켜집니다.

확정 기기 바뀜(폐기·`crew.set_committer`·소유권 이전·관리자 역할 빠짐): 해소기는 확정 기기가 자격을 잃으면 결정론적으로
넘깁니다(소유자의 가장 작은 활성 기기, 소유자에게 활성 기기가 없으면 관리자 활성 기기 중 가장 작은 것).

- 표 넘기기(`maybe_handover` → `send_handover`): 옛 확정 기기는 표를 새 확정 기기(와 예비 기기)에게 `seal_handover`로 봉인해
  `YpRequest::KeyTableHandover`로 보냅니다(크루와 메타데이터 하나, 그다음 그룹마다 하나, `seal_part`). 받는 쪽(`receive_handover`)은
  보낸 기기가 지금도 표를 가질 자격(`holder_ok`: 해소기의 표 보유 기기이면서 소유자·관리자의 활성 기기)이 있고 크루 전체 재키
  요구가 없으며 내가 지금 확정 기기나 예비 기기일 때만 열고(`open_handover`: 서명 영역 `KeyTable`, 표 자체 검사), 크루 요청은 크루
  사슬 끝이 내가 아는 것과 같을 때만 씁니다(이미 맞는 표가 있으면 그대로). 그룹 요청은 `committer::merge_groups`(지금 표의 크루와
  `check_private_against`, 더 뒤 순번만). 크루를 받은 뒤 `key_handover_wait` 동안은 아직 오지 않은 그룹을 기다립니다
  (`KeyGroupState::received`). 표와 함께 온 공개 메타데이터(`KeyTableHandover::meta`: 마지막 전체 재키 시각·채널 지도·R 할 일)는
  `committer::adopt_meta`로 저장합니다(시각은 지금으로 자르고 R 할 일은 합침). 확정 기기 변경을 막 낸 옛 확정 기기의 첫 넘기기는 새 확정 기기가
  아직 그 변경을 몰라 거절되기 쉬워, 받는 쪽은 거절하면서 보낸 기기와 따라잡기를 돌리고(`spawn_catch_up`) 보낸 쪽은
  `key_handover_wait` 안에서 0.25초부터 두 배씩(4초까지) 다시 보냅니다(`handover_still_due`: 확정 기기·사슬 끝이 그대로일 때만).
  넘긴 기기는 표를 버리지만 보유 기기로 남아, 그 기기가 소유자·관리자의 활성 기기가 아니게 되면 다음 크루 갱신은
  `FullRekey`입니다. 해소된 상태가 그대로이고 크루 갱신만 새로 적용돼도(`IngestReport::key_chain_moved`) 확정 기기가 아닌 기기는
  `keygroup::on_key_chain_moved`로 들고 있던 표가 낡았는지 봅니다(다른 기기가 이어 냈으면 버림).
- 표 없이 확정 기기가 되면 위 1처럼 기다렸다 공개 끝으로 이어받아 전체 재키합니다(`handover_wait`).
- 크루 밖의 확정 기기(`maybe_member_handover` → `hand_state_keys`): 확정 기기가 크루 사슬 끝 멤버가 아니면(표도 크루 키도 없이
  확정 기기가 된 기기) 크루 사슬 끝의 가장 작은 활성 기기가 바로, 다른 기기는 `key_handover_wait` 뒤에 최근 상태 키와 지금 크루 R을 그
  사용자의 받은편지함에 봉인해 넘깁니다. 받는 쪽은 그 상태 키를 저장만 하고 지금 키로 쓰지 않습니다(`state_key_handover_only`).
  새 확정 기기는 상태를 세운 뒤 `GetGroupKeys{crew}`로 사슬 끝 T·R을 받아 전체 재키를 냅니다. (공간, 확정 기기, 크루 순번)마다 한 번.
- `crew.set_committer`: 소유자 기기가 `CommitterChange`를 냅니다(대상은 소유자·관리자의 활성 기기, 1:1 공간에서도 씀).
- 예비 기기(`KeyBackupSet`)는 형식·해소기 검사와 표 넘기기의 받는 쪽만 있고, 정하는 로컬 API는 아직 없습니다(decisions.md "키 갱신 3부").

## 보내는 쪽 규칙과 놓친 키

- 일반 채널에 보낼 때(`messages::send_key_locked`) 뿌리는 채널이 쓰는 그룹(`keygroup::channel_group`)의 정규 사슬 끝에서 파생합니다
  (`member::root_for`: 기록 공개면 R의 공개 뿌리, 아니면 T의 비공개 뿌리). 그 그룹 키를 아직 얻지 못했으면 크루 키로 내려가지
  않고 `1010`(키 갱신 대기)입니다. 따로 내는 뿌리 묶음은 없습니다.
- 그 전에 `keygroup::wait_rotation`: `removal_pending`(해소기의 빚·전체 재키 요구, 그룹 사슬에 쓸 수 있는 끝이 없음, 크루 전체 재키
  뒤 그룹이 아직 새 T를 내지 않음, 또는 그룹 사슬 끝 멤버 가운데 이제 그 채널을 볼 수 없는 사용자의 기기)이면 100ms마다 다시 보며 `rotation_wait`(기본 30초, 시험 설정 2초)까지 기다리고, 크루 설정
  `strict_rotation`이면 끝없이 기다립니다. 시간이 다 되면 옛 키로 보내고 공간을 `rotation_pending`에 올려 `space.get`의
  `key_rotation_pending`을 켭니다(`space.updated` 알림, 회전이 오면 꺼짐). 내가 확정 기기면 기다리는 동안 바퀴를 바로 돌립니다.
- 받은 뿌리(키 묶음·중계)는 뿌리 저장소(`keys::put_root`, 범위 공간‖채널)에 두고 읽기에만 씁니다.
- 놓친 키 중계(`keygroup::relay`): 모르는 뿌리 ID의 메시지는 (작성 기기, 채널)마다 예산(`MemberState::may_look_up_root`: 한 번에
  16번, 그 뒤 10초에 한 번) 안에서만 파생해 보고, 찾지 못한 뿌리는 새 그룹 상태·뿌리를 얻을 때까지 기억합니다(`missed_root`).
  찾지 못한 메시지는 작성자가 그 채널을 볼 수 있을 때만(`VIEW_CHANNEL`, 못 보면 파생 전에 거절) 키 대기(`Need::Record`)에 두고 예산 안이면 같은 공간 멤버 3곳에
  `GetChannelRoots{space, channel, ids ≤ 512}`를 묻습니다(`ask_roots`, 같은 것은 20초 안에 다시 묻지 않음). 답(`handle_get_roots`)은
  활성 멤버 기기 → 채널 열람 → 기록 공개(아니면 `HistoryPrivate`) → 가진 뿌리 순으로 확인하고 요청 기기의 상자 키로 봉인한
  `SealedKeys`(AD = `"yangpa/v3/relayed-keys" ‖ 공간 ‖ 요청 기기`)입니다. 받은 쪽(`take_roots`)은 뿌리에서 계산한 ID가 물은 채널의
  물은 ID일 때만 저장하고 기다리던 메시지를 깨웁니다. 사슬 끝인데 몫을 얻지 못한 그룹(`MemberGroup::stuck`)은
  `GetGroupKeys{space, group}`(`ask_group`)로 지금 T·R을 묻고 뿌리 약속과 맞을 때만 받습니다. 체크포인트 키 머리에서 세워
  바닥 멤버를 모르는 그룹은 그 그룹을 볼 수 있는 멤버에게 `GetGroupMembers{space, group, seq, root_commit}`(`ask_members`, 중계
  4판)로 그 순번 멤버 목록을 받아 그룹 사슬에 알려 줍니다(`MemberState::learn_members`). 사슬은 머리의 멤버 요약과 맞는 목록만
  받으므로 누가 답했든 같습니다(protocol.md "키 그룹과 체크포인트"). 멤버를 모르는 동안 그 그룹 키는 중계하지 않습니다.
- 옛 상태 키(`ask_state_keys`): 적용 중 크루 상태 키를 몰라 기다리게 된 공간 스트림 이벤트(상태 이벤트·크루 갱신·공간 스트림
  키 묶음)의 키 ID를 모아 정리 끝에 넘기고, 1초 뒤에도 기다리는 것만 `GetStateKeys{space, ids ≤ 512}`로 묻습니다. 묻는 곳은
  상태가 있으면 활성 멤버 기기 3곳, 가입 중이라 상태가 없으면 받아 준 노드(`crews::join_peer_ok`)와 내 다른 기기입니다. 답
  (`handle_get_state_keys`)은 활성 멤버 기기에게만 가진 크루 상태 키(`keys::crew_state_key`, 그룹 상태 키는 아님)를 봉인합니다.
  받은 쪽(`take_state_keys`)은 키에서 계산한 ID가 물은 ID인 것만 지금 키 후보로 올리지 않고 저장하고(`put_state_key(promote =
  false)`) 기다리던 이벤트를 깨웁니다. 그래서 가입 묶음은 최근 상태 키 64개로 충분하고, 크루가 오래돼도 가입자가 옛 상태
  이벤트를 영영 기다리지 않습니다.

## 블롭과 정책 (B3 설계)

- **첨부 보내기**(`blobs::prepare_attachments`): 원본은 파일 경로 또는 `blob.paste`로 올린 붙여넣기(`Origin`, 붙여넣기는 `paste.rs`의
  RAM 버퍼를 빌려 `Source::Mem`으로 읽음)이고, 하나의 한도는 이 기기 `sharing.max_attachment_bytes`(기본 무제한)와 채널
  `attach_limit` 중 작은 쪽(`send_limit`, 넘으면 원본을 읽기 전에 `1005`)입니다. 원본을 두 번 읽습니다. 처음엔 조각마다 암호화해 매니페스트(블롭 ID)를 만들고,
  `blob_register` 뒤 다시 처음부터 읽어 같은 키·논스로 암호화한 조각을 `blob_write_chunk`(해시가 매니페스트와 다르면 파일이 바뀐
  것이므로 `1010`). 보내는 블롭은 등록하자마자, 조각을 쓰기 전에 고정(pin)합니다(암호화하는 동안 저장 공간 정리가 정리 대상으로
  보고 비우지 않게. 이모지·`import_bytes`도 같음). `keep_metadata`가 아니면 원본을 열 때(`Origin::open`) `blobs/strip.rs`가
  JPEG·PNG·WebP·GIF·MP4·MOV의 메타데이터를 뺀 읽기(`Spliced`)로 바꾸고, 이후 판별·썸네일·크기·암호화는 모두 그 내용을 씁니다
  (휘발성 첨부도 같음, decisions.md "첨부 메타데이터"). 메시지에 실리는 것은 블롭 ID·키·크기·MIME(내용으로 판별)·이미지 크기·썸네일·재생 길이.
  썸네일(`blobs/media.rs`): 32KiB 이하 png·jpeg·gif·webp는 원본 그대로, 더 큰 것(50MiB까지)은 풀어서 긴 변 320px 이하 JPEG
  (48KiB를 넘으면 240·160px·낮은 품질로 다시, 투명은 흰 바탕, EXIF 회전 반영. 이미지 크기도 회전을 반영한 값). 풀 때 한 변 16384px·
  할당 256MiB 한도. 재생 길이는 MP4·MOV(`moov`/`mvhd`)만. 발행이 실패하면 준비한 블롭을 지웁니다(`discard_prepared`).
  동영상·음성은 앱이 넘긴 미디어 힌트(`AttachmentInput`의 썸네일·가로·세로·길이)를 `blobs/hints.rs`가 검사합니다: 파일을 읽기 전에
  base64·바이트 수·형식(PNG·JPEG·WebP, 64KiB)·범위를 보고(잘못 만든 값은 `1005`), 내용 판별 뒤 이미지이거나 동영상·음성으로 가리지
  못하면 힌트만 버리고 보냅니다(앱과 데몬의 판별이 다를 수 있어 보내기를 막지 않음. 음성이면 가로·세로만 버림). 판별은 앱의 Tauri
  쪽(`preview.rs`의 `sniff_media`)과 같은 규칙으로 ID3·머리말 없는 MP3(`audio/mpeg`)·AAC ADTS(`audio/aac`)도 음성으로 봅니다. 썸네일은
  긴 변 640px·할당 16MiB 한도로 끝까지 풀어 이미지 썸네일과 같은 `make_thumbnail`로 JPEG를 다시 만듭니다(앱이 넣은 메타데이터·다른
  형식이 받는 쪽에 가지 않게). MP4·MOV에서 읽은 길이가 있으면 앱 값보다 우선합니다.
- **쓰임 기록**: 받은(또는 보낸) 메시지의 첨부마다 `blob.ref.<블롭>`에 (공간, 채널, 이벤트, 논리 시계, 종류, 크기, 키 확인 결과)를
  남깁니다(블롭마다 8개). 기록할 때 가진 조각 하나를 그 메시지의 키로 풀어 보고(`key_check`, 결과는 (블롭, 키 지문)으로 RAM에 기억),
  풀리지 않으면 기록하지 않습니다. 조각이 아직 없으면 '모름'으로 두었다가 조각이 생긴 뒤 쓸 때 확인합니다(`verified_refs`).
  가득 차면 틀린 기록 → 확인 안 된 가장 새 기록(첫 기록 제외) 순으로 밀어내고, 그래도 없으면 새 기록을 버립니다.
  로컬 API용 쓰임(`uses_in_space`) = 이 기록 + 공간 이모지 + 공간 아이콘 + 멤버 프로필 이미지. 키도 쓰임에서 찾습니다(`key_for`).
- **제공**(`blobs/serve.rs`): 인증 → 공간 멤버의 활성 기기 → 내가 가진 블롭 → 그 공간에서 믿을 수 있는 쓰임(`serve_uses`, 없으면
  있는지도 알리지 않고 `NotFound`) → 쓰임마다 시더 검사, 하나라도 통과하면 제공. 믿을 수 있는 쓰임은 블롭이 그 공간에 등록돼 있고
  (`blob_spaces`: 내가 그 공간에 올렸거나 그 공간을 적어 받은 블롭) 키가 맞는 것: 키 확인을 통과한 첨부 기록, 키가 맞는 공간 이모지,
  키가 맞는 공간 아이콘(`SpaceSettings::icon_key`). 프로필 이미지는 `PROFILE_SPACE`에 등록돼 있고, 그 공간 멤버의 서명된 프로필이 가리키며
  그 키로 풀릴 때만(멤버 프로필 훑기는 이 경우에만 하므로 보통 요청은 SQLite를 훑지 않음). 그래서 다른 공간의 블롭 ID를 메시지·
  프로필에 베껴 적어도 이 공간 멤버에게 내주지 않습니다. 첨부는 메시지 이벤트 요청과 같은 `check_seed_request`(채널 권한, 입장 이전
  기록 공개, 제공 범위·개별 메시지 제외, 동시 전송·대역폭). 지워진 메시지·휘발성 채널의 첨부는 제공하지 않습니다.
  이모지는 채널 없는 요청으로, 아이콘·프로필 이미지는 시더 켜짐·이미지 종류·동시 전송·대역폭만. 매니페스트·비트맵 요청도 같은 검사.
- **받기**(`blobs/fetch.rs`, 블롭마다 작업 하나, 동시 3개): 매니페스트(저장소 → 멤버 기기들에게 동시에 물어 ID·형식 확인) →
  자동이면 메시지에 적힌 크기 + 64KiB를 넘는 블롭은 포기 → 저장소 비트맵으로 `ChunkScheduler`를 이어 시작 → 멤버 기기마다
  `BlobHave`(받을 수 있는 조각이 없으면 2초, 있으면 10초마다 다시) → 희귀한 조각부터 여러 시더에게 동시에 `BlobChunk` → 조각 해시 검증 후 저장·
  `blob.progress`. 검증 실패는 그 시더의 점수를 깎고(`BadData`, 연결을 끊을 만큼 깎이면 `net.penalize`) 그 조각을 다른 시더에게
  다시 요청, 3번 틀린 시더는 그 블롭에서 뺍니다. 15초 동안 진전이 없으면 나중에 다시(5초부터 두 배씩 최대 5분, 자동은 12번이면 포기).
  자동 시도가 실패하면 같은 블롭을 쓰는 다른 공간의 기록(키가 틀리지 않았고, 휘발성·휴면·지운 메시지가 아니며 피어 설정이 허용하는
  것)으로 넘어가 다시 시도합니다(`next_space`). 넘어갈 곳이 없을 때만 포기. 자동 시도 중에 `blob.fetch`가 오면 그 시도가 끝난 뒤
  수동 요청으로 바로 다시 넣습니다(자동 시도의 실패로 버리지 않음). 받은 블롭은 적은 공간에 등록하고, 이모지 고정은 새로 받았을 때만.
  수신 대역폭은 피어 설정의 `download_limit_bps`(공간 값이 있으면 공간마다 따로)와 범위별 받기 제한(`ratelimit.rs`, 첨부의 크루·채널과
  조각을 줄 사람)을 모두 지켜야 요청합니다(`blobs::admit_chunks`). 송신은 `sync_service::check`에서 전체 상한과 범위별 올리기 제한을 함께 봅니다.
- **전송 속도**(`transfer.rs`): `yangpa_net::Traffic`(모든 스웜의 코덱이 세는 요청·응답 바이트)을 1초마다 읽어 3초 평균을 내고,
  내준 첨부 조각을 (블롭, 받는 사람)마다 더해 `transfer.rates` 알림을 값이 바뀔 때만 보냅니다. 메모리에만 둡니다.
- **다시 잇기**(`blobs/prepare.rs` `relink`): 첨부의 키·조각 크기로 로컬 파일을 다시 암호화해 블롭 ID가 같을 때만 조각을 씁니다(쓰는 동안만
  저장소 고정, 끝나면 사용자 고정).
- **받을 목록**(`blob.want.<블롭>`, 재시작해도 유지): 우선순위 대기열(`yangpa_sync::priority`, 보고 있는 채널 먼저, 종류별 우선순위) +
  수동 요청은 그보다 먼저. 자동 내려받기는 다른 기기가 보낸 첨부, 휴면이 아닌 공간, 피어 설정의 종류·크기 한도와 채널 `attach_limit`
  안일 때만(미리 받기 `prefetch::backfill`도 채널 한도를 넘는 첨부는 뺌).
  열지 않은 크루 채널의 첨부는 그 채널을 열 때까지 미룹니다. 휴면 공간의 자동 내려받기는 꺼내 두었다가 깨어나면 돌려놓습니다.
  한도: 바로 받을 자동 항목 10,000개(미룬 것은 세지 않음), 미룬 항목은 전체 10,000개·공간마다 1,000개(넘으면 그 공간의, 없으면
  전체의 가장 오래된 미룬 항목부터 버림). 공간이 휴면이 되면 그 공간의 미룬 항목은 버립니다(열지 않은 채 쌓이지 않게. 그 첨부는 `blob.fetch`로 받음).
- **주기 작업**(60초): 프로필 이미지 색인을 다시 세우고, 휴면이 아닌 공간의 이모지·아이콘과 친구(1:1 공간)의 아바타·배너를 받을 목록에
  넣습니다(크루 멤버 프로필 이미지는 `blob.fetch`로). 이 항목은 피어 설정의 이미지 종류만 따르고 `auto_download` 크기 한도는 보지 않습니다
  (프로필 이미지 한도 8MiB가 Tor 기본 2MiB보다 커서 친구 아바타가 영영 안 오던 문제). 받은 프로필이 바뀌면 `on_profile_changed`가
  색인을 고치고 그 친구의 새 이미지를 바로 받을 목록에 넣습니다(다음 주기를 기다리지 않게). 10번에 한 번 정리: 공간 이모지·아이콘으로 쓰이는 블롭과 프로필 이미지
  블롭(`PROFILE_SPACE`)은 건드리지 않고, 가진 블롭은 믿을 수 있는 기록(키 확인을 통과했고 그 공간에 등록됨)이 모두 지워진
  메시지이거나 없는 공간일 때만, 아직 없는 블롭은 틀리지 않은 기록이 모두 그럴 때만 지웁니다. 믿을 기록이 하나도 없으면 남깁니다
  (남이 내 블롭 ID를 베낀 메시지를 지워 내 블롭을 지우게 하지 못하게).
- **저장 공간 정리**(`blobs/cache.rs`, X4): 조각 데이터를 가진 블롭을 셋으로 나눕니다. 늘 남김(저장소 `pinned`: 내가 올린 첨부·
  이모지·아이콘·프로필 이미지·받은 이모지, `PROFILE_SPACE` 블롭, 아는 공간의 이모지·아이콘), 고정(`blob.pin`, 설정
  `blob.pin.<블롭>` — 저장소 `pinned`와 따로 둬 풀어도 내가 올린 첨부가 정리 대상이 되지 않음), 정리 대상(나머지). 한도
  (`blob.cache.limit`, 없으면 `--cache-limit`·기본 2GiB)는 정리 대상에만, 최소 여유(`blob.cache.min_free`, 기본 1GiB)는 데이터 경로
  디스크(Unix `statvfs`, Windows `GetDiskFreeSpaceExW`)에 겁니다. 정리 대상을 `last_access`(받은 조각을 쓸 때, 제공·`blob.read` 때
  갱신) 오래된 순으로 `Store::blob_drop_data`(비트맵을 0으로, 등록·매니페스트·공간 소속은 남김)로 비웁니다. 받는 중·받을 목록의
  블롭과 30초 안에 남에게 내주거나 로컬 API로 읽은 블롭(`cache::touch`)은 건너뜁니다. 언제: 받기 작업이 매니페스트를 안 뒤
  (`admit_download`: 남은 바이트만큼 자리를 만들고, 못 만들면 자동은 `NoRoom`으로 그만두고 수동은 그대로 받음. 늘 남기거나 고정한
  블롭·채널 없는 받기는 한도를 보지 않고 디스크 여유만), 60초 주기, 한도를 바꿀 때(`enforce`), `blob.cache_clear`. 블롭마다 저장소에
  쓰고 파일을 지우므로 모두 `spawn_blocking`에서 돌립니다(`cache::run_blocking`: API·받기 작업·`seed.set`의 보충, 주기 작업은 통째로).
  목록을 읽은 뒤 들어온 `blob.pin`을 놓치지 않게, 비우기 직전에 고정 목록 읽기 잠금을 쥔 채 다시 보고 비웁니다. 비운 블롭에
  `BlobHave`는 빈 비트맵으로 답하고(키 확인을 아직 못 한 쓰임만 있으면 남은 조각이 없어 `NotFound`), `seed.coverage`·남의
  `seeders_online`이 사실대로 줄어듭니다. 저장소 행이 남아 쓰임 정리(위)가 나중에 지울 수 있고, 다시 받으면 처음부터 받습니다.
- **미리 받기**(`blobs/prefetch.rs`, 시더 설정 `prefetch`, X4): 켜면 열지 않은 채널도 제공 범위가 되고(`SeederPolicy::seeds_channel`),
  `on_message_applied`가 그 채널의 새 첨부를 미루지 않습니다(`policy::prefetches`: 시더 켜짐·제공 범위·휘발성 아님·`VIEW_CHANNEL`).
  "새 첨부"는 작성 시각이 `prefetch::LIVE_MS`(10분) 안인 메시지의 것만입니다. 오래 꺼져 있다 따라잡기로 들어온 지난 메시지의 첨부는
  미뤄 두어, 한도가 찬 시더가 지난 첨부를 하나씩 받으며 다른 첨부를 비우는 일을 되풀이하지 않게 합니다(아래 보충이 자리만큼 받음).
  지난 첨부는 60초 주기와 `seed.set` 때 `backfill`이 저장소 행이 없는(한 번도 받지 않은) 블롭만 최근 것부터, `room_for_backfill`
  (한도 − 정리 대상 − 받을 자동 항목, 디스크 여유 반영) 안에서 64개까지 넣습니다. 이 항목(`Want::backfill`)은 자리를 만들려고 다른
  첨부를 비우지 않고, 비운 블롭은 행이 남아 다시 넣지 않으므로 받고 비우기를 되풀이하지 않습니다. 미룬 항목을 보충이 깨우면
  보충 표시를 이어받습니다(`add_want`).
- **프로필 이미지**: 공간에 속하지 않으므로 가상의 공간 `PROFILE_SPACE`(0)에 등록하고 키는 비밀값 `blob.image.<블롭>`과 서명된
  프로필의 `avatar_key`·`banner_key`(이진 형식에만 실림, JSON에는 늘 `null`). 친구·같은 공간 멤버가 공간을 정해 요청합니다.
- **링크 미리보기**(`blobs/preview.rs`): 보내는 쪽만 조회합니다. rustls(webpki 루트), 이름을 직접 풀어 공인 주소만 연결(사설·루프백·
  링크 로컬·CGNAT·문서용 등 거절, 검사한 주소로 연결, 넘겨주기마다 다시 검사), 포트 80·443·8080·8443, 링크 3개·5초·HTML 512KiB·
  넘겨주기 3번. 이미지는 2MiB까지 받아 64KiB를 넘으면 긴 변 320px 이하 JPEG로 줄임(별도 스레드, 한 변 8192px·할당 64MiB 한도).
  시험용 `allow_private_addrs`는 주소 검사만, `allow_any_port`는 포트 제한만 끔. Tor로 도는 기기에서는 만들지 않음
  (`tor::tor_active`: `--direct`가 아니면 늘 참, 시험은 `simulate_tor`로 직접 연결 스웜도 Tor처럼 봄). 시더·피어 기본값의 Tor 여부도 망의 실제 상태를 봄.
- **정책**(`policy.rs`): 설정 표의 `seed.policy[.<공간>]`, `peer.policy[.<공간>]`, `seed.excluded.<공간>`, `policy.opened.<공간>`,
  `policy.active.<공간>`(한 시간에 한 번만 씀). 공간 값이 있으면 전역 값을 대신합니다.
- **휴면**: 열기·보내기·직접 받은 전파가 `dormant_after`(기본 14일) 동안 없으면 주기 따라잡기에서 휴면으로 표시합니다. 휴면 공간은
  gossip 구독을 끊고, 주기·연결 직후 따라잡기에서 빠지며, 자동 내려받기를 멈춥니다. gossip으로 받은 것은 활동으로 세지 않아,
  열지 않는 바쁜 크루도 휴면이 됩니다. 열거나 직접 전파가 오면 깨어나 구독을 되살리고 인증된 멤버 기기마다 따라잡습니다.
  휴면이 되지 않는 공간(`never_dormant`): 1:1 스페이스(친구 DM을 오프라인 동안 놓치지 않게), 내 기기가 확정 기기인 크루(키 갱신·
  가입 처리가 멈추지 않게), 시더 설정이 켜져 있고 모든 채널(`all_channels`)이나 채널 목록을 제공하거나 미리 받기(`prefetch`)를 켠 공간(머리 없는 시더).
  예전에 휴면으로 표시됐어도 주기 따라잡기에서 깨웁니다. `dormant_after`가 0이면 휴면을 쓰지 않습니다.

## 휘발성 채널 (`volatile`)

데이터는 모두 RAM에만 있습니다. 암호문과 세션 키는 `VolatileStore`(직렬화 불가, 해제 때 0으로 덮어씀, 키는 잠긴 메모리),
이벤트 머리 정보(종류·논리 시계·작성자·세대·대상, 내용 없음)·참가자·세대·읽음 위치는 `VolatileState.chans`(채널별 `ChanRt`).
디스크에 남는 것은 `channel.allow_api_read` 허용 표시(설정 `volatile.api_read.<공간>.<채널>`, ID만)뿐입니다. 로그에는
피어·오류만 남기고 내용·키·세대는 남기지 않습니다. 잠금 순서는 SpaceCore → `chans` → 저장소·피어 목록이며, 알림·갱신·
스냅숏 요청·입장 알림은 잠금을 푼 뒤(`Effects`) 합니다.

- **참가자**(이 기기가 참가 중일 때만 셈, 나 포함):
  - 입장·퇴장 방식: `channel.join` 한 이 기기 + `VolatilePresence(true)`를 주고받은 인증된 기기. 입장하면 채널을 볼 수 있는
    인증된 멤버 기기 모두에게 알리고 답을 기다립니다. 이미 들어가 있는 기기는 자기 입장으로 답을 보낸 **뒤에** 갱신을 시작합니다
    (새 참가자가 갱신 키보다 참가자 목록을 먼저 알게). 1초 주기 작업이 연결 끊김·권한 잃음을 빼고, 새로 인증된(다시 연결된)
    기기와 알림(입장·답)이 실패한 기기에 입장을 다시 알립니다(`announce`가 실패하면 `announced`에서 뺌). 상시 채널에
    `channel.join`·`leave`는 `1005`.
  - 상시: 채널을 볼 수 있는 인증된 온라인 멤버 기기(1초마다, 그리고 메시지 API를 부를 때 다시 셈).
  - 피어 연결이 끊기면(`on_peer_disconnected`) 바로 뺍니다.
- **세션 키**:
  - 참가자가 바뀌면 참가자 중 기기 ID가 가장 작은 기기(갱신 담당, `rekey_once`)가 무작위 64비트 세대를 냅니다. 부모(지금
    보내는 세대)의 받는 기기가 모두 아직 참가자면 가입 회전(`vol_join`으로 유도, 가입자에게만 감싸기), 아니면 새 무작위 키를
    모두에게 감쌉니다(`FRESH_EVERY` 1초에 한 번, 그 사이 바뀐 참가자는 기다렸다가 함께). 공지는 `VolatileRotation`(키 없음,
    `KeyChoice::Volatile(0, vol_notice_key)`), 감싸기는 `VolatileKeyWrap` 요청으로 공지보다 먼저 보냅니다. 같은 참가자 집합으로는
    다시 갱신하지 않습니다(`rotated_for`). 직접 받은 `KeyShare`는 쓰지 않고, 내가 요청한 스냅숏 첫 항목의 `for_user: Some(나)`
    묶음만 받습니다.
  - 받은 공지(`rotation` → `try_rotation`): 받는 기기 목록을 `gen_recipients`(세대 → 받는 기기, 256세대)에서 더한·뺀 목록으로
    세거나 감싸기의 목록, 둘 다 없으면 내 참가자 목록을 요약과 맞춤. 키는 부모 키에서 유도하거나 공지를 쓴 기기의 감싸기
    (`wraps`, 세대당 보낸 기기마다 한 자리, 4개, 4KiB)를 열어 약속값과 맞춤. 아직 키가 없으면 `pending_rot`(64세대)에 두고
    `keys_arrived`·`handle_key_wrap`이 다시 시도. 둘 다 `EARLY_TTL`(30초) 뒤 정리.
  - 키를 잃은 참가자: 담당이 아닌데 담당의 갱신 키 없이 3초(`STALE_AFTER`, 1초 주기 `check_stale`)가 지나면 강제 갱신을 냅니다.
    담당은 자기 지금 키를 받았던 기기의 갱신을 받으면 `need_fresh`로 새 무작위 키를 냅니다(빨리 다시 시작한 기기).
  - 상시 참여 상한(`trim_to_cap`, `SpaceState::volatile_cap`): 온라인 시청 기기와 나를 합쳐 상한을 넘으면 기기 ID가 작은 순으로
    상한만큼만 `present`에 남깁니다. 내가 밀려나면 `full`(그 상한)이 서고 참가하지 않은 것으로 보며(`participating` 거짓),
    `shed_if_full`이 받은 이벤트·키를 지웁니다. 보내기는 `full_error`(`1010`). `ChannelView.volatile_cap`·`volatile_full`을 채웁니다.
  - 갱신 전달(`deliver_key`): 기기마다 따로 보내고, 실패하면 그 갱신이 아직 내 가장 새 갱신이고 그 기기가 같은 연결로
    참가 중인 동안 200ms부터 두 배씩(최대 5초) 6번까지 다시 보냅니다(모든 참가자가 같은 키로 모임). 끝내 못 보내도 참가자에서
    빼지 않습니다(한쪽만 빼면 그 기기는 자기가 아직 참가자인 줄 알아 다시 알리지 않으므로 서로 다른 참가자 목록에 머묾). 연결이
    끊기면 1초 주기 작업이 빼고 새 갱신이 나갑니다.
  - 저장소(`VolatileStore`): 키 넣기는 세대 수에 비례하지 않습니다(안 쓰는 키 정리는 1초 주기의 `expire`). 채널당 세대 키
    1024개(`max_generations_per_channel`)를 넘으면 안 쓰는 세대부터, 그다음 가장 오래 쓰이지 않은 세대를 그 세대 이벤트와 함께
    밀어내고(다음 `expire`가 `message.expired`로 알림), 빈 잠긴 보관소는 해제합니다.
  - 보낼 세대: 갱신 담당의 가장 새 갱신(논리 시계·작성 시각·ID 순). 단 그 세대를 받은 기기(공지로 센 목록)가
    모두 지금 참가자일 때만. 아니면 지금 키가 그 조건을 아직 만족하면 유지, 아니면 없음(떠난 기기가 가진 키로 새 메시지를
    암호화하지 않음). 저장소의 `make_current`로 그 세대를 지금 키로 표시해 정리에서 지켜 줍니다.
  - 보낼 때: 받은 기기가 지금 참가자와 정확히 같은 키를 최대 2초 기다리고(막 들어온 참가자도 읽게) 그 뒤에는 있는 키로 보냅니다.
    키가 아예 없으면 5초 뒤 이 기기가 직접 갱신하고, 10초가 지나도 없으면 `1010`.
- **받기**(`on_inbound_event`, 적용 잠금 안이라 동기로 짧게): 채널을 모르면 무시(보류할 곳이 없음) → 작성자가 멤버의 활성 기기 →
  내가 볼 수 있는 채널 → 참가 중 → 작성 기기가 지금 참가자(`author_present`, 상시 채널은 그 자리에서 다시 셈. 입장·퇴장 방식에서
  아직 입장을 모르는 기기의 것은 `ChanRt.early`에 30초·256개·4MiB까지 두었다가 그 기기의 입장을 받으면 키 묶음부터 처리. 나간 것을 본 기기(`present: false`·연결 끊김·권한 잃음)의 것은 다시 입장할 때까지 30초간 두지도 않음) → 키 레인이면 회전 공지는 위 `try_rotation`, 스냅숏 키 묶음은 봉인을 열고 `authorize_message` 뒤 키를 넣고, 그 키를 기다리던 이벤트를 다시 봄,
  메시지 레인(`KeyRef::Volatile`만)이면 암호문을 RAM에 넣고 세션 키로 풀어 대상(같은 채널의 `Message`) 확인 → `authorize_message` →
  보기에 넣고 `message.new`/`message.updated`. 평문은 쓰자마자 덮어씁니다. 키·대상을 기다리는 이벤트는 10분 뒤 버립니다.
  확인을 마친 이벤트만 머리(`head`, 다음 휘발성 이벤트의 추가 부모)에 반영하고, 부모 논리 시계는 저장된 공간 최댓값 +
  `VOLATILE_LAMPORT_LEAD`(2^19)에서 자릅니다. 보기 순서는 (논리 시계, 작성 시각, ID).
- **전달**: 참가자 노드에만 `PushEvents`. `sync_service::on_gossip`은 `is_direct_only` 이벤트가 섞인 묶음을 통째로 거부하고(전달도 안 함), 따라잡기·시더 응답은
  저장되지 않은 이벤트를 모르므로 내보내지 않습니다.
- **수명**: 이 기기가 받은 시각부터 `ttl` 초. 1초 주기 작업이 만료를 지우고 `message.expired`(그 메시지를 고친·반응한 이벤트도
  같이 지움). 지운·만료된 ID는 15분(최대 65536개) 기억해 늦게 온 사본·스냅숏이 되살리지 않게 합니다. `ttl_on_leave`면 작성
  기기가 참가자에서 빠질 때 남은 기기들도 그 기기가 쓴 이벤트를 지우고 `message.expired`를 알립니다.
- **퇴장·종료**: `channel.leave`는 그 채널의 암호문·키·머리 정보·읽음 위치를 바로 지우고 참가자에게 `VolatilePresence(false)`.
  종료는 `wipe_all`(위 "시작과 종료"), 1초 주기 작업도 종료 신호를 받으면 지웁니다. 채널이 지워지거나 볼 권한을 잃으면 다음 주기에 지움.
- **늦은 입장자**: 과거 기록 공개면 입장 직후(상시 채널은 혼자였다가 다른 참가자가 보일 때) 참가자 모두에게(동시에 4개씩)
  `VolatileSnapshot`을 보내 받은 `VolatileEvents`를 모두 적용합니다(같은 이벤트는 한 번만). 항목은 `accept`의 스냅숏 검사를 거칩니다:
  공간 최댓값 + 2^20을 넘는 논리 시계는 버림, 키 묶음은 응답 기기가 만든 것만, 수명 '퇴장 시'면 작성 기기가 지금 참가자인 메시지만. 첫 항목은 응답자가 요청 기기에 봉인한 키 묶음 `KeyShare{for_user: Some(요청자)}`(갱신으로
  치지 않음), 나머지는 수명이 남은 이벤트와 `remaining_ms`(최근 것부터 511개·6MiB까지). 스냅숏으로 받은 메시지는 알림을 내지
  않습니다. 비공개면 응답자가 `HistoryPrivate`로 거절하고 새 참가자는 입장 뒤 갱신된 키만 받습니다.
- **첨부**(`volatile/attach.rs`, X1): 디스크 블롭 저장소·DB·임시 파일을 거치지 않습니다.
  - 보내기: `message.send`가 권한(ATTACH_FILES, 없으면 파일을 읽기 전에 `1003`)·개수·크기(`NodeConfig::volatile.max_attachment_bytes`
    (기본 무제한)와 채널 `attach_limit` 중 작은 쪽(`channel_max`), `1005`)·RAM 예산(`max_attachment_ram_bytes`, `1010`)을 먼저 보고,
    `spawn_blocking`에서 파일(또는 붙여넣기 RAM 버퍼, `blobs::Origin`)을 한 번 읽어 머리 64KiB로 형식·이미지
    크기를, 이미지는 메모리에서 썸네일을(일반 첨부와 같은 규칙), MP4·MOV는 재생 길이를 만든 뒤 첨부마다 새 키로 조각 암호화해
    `VolatileStore::stage_blob`에 올립니다(평문은 암호화기의 조각 버퍼에만). 메시지를 적용한 뒤 참가자에게 보내기 **전에**
    `claim_blob`으로 그 메시지에 묶습니다. 묶기 전의 첨부는 저장소가 시간으로 지우지 않고(큰 파일 여러 개를 천천히 읽는 동안
    먼저 올린 것이 사라지지 않게) `attach::Staged`가 쥐고 있다가, `send_message`가 어떻게 끝나든(실패·호출 취소·읽기 작업의
    패닉) 묶이지 않은 것을 `remove_staged`로 지웁니다. 하나라도 묶지 못하면(그새 메시지가 밀려났거나 채널을 나감) 메시지를
    참가자에게 보내지 않고 이 기기에서도 거두며(`retract`, 보던 화면에는 `message.expired`) `1010`.
  - 받기: `try_ready`가 메시지를 풀어 확인하면 첨부를 채널 색인(`ChanRt.attach`, 블롭 → 처음 실은 메시지)에 넣고, 잠금을 푼 뒤
    (`Effects`, 새 메시지 알림보다 먼저) 자동 받기를 정합니다: 내 기기·차단한 사용자·휴면 공간이 아니고 피어 설정(Tor로 도는 기기면 Tor
    기본값)의 종류·자동 한도와 채널 한도(`channel_max`) 안. 받을 첨부는 노드 하나의 RAM 대기열(상한 256개, 가득 차면 아직 아무것도 받지 못한 가장 오래된
    자동 요청을 밀어냄, 직접 요청은 밀려나지 않고 밀어낼 것이 없으면 `blob.fetch`가 `1010`)에 넣고, 일꾼 하나(`attach::start`)가
    직접 요청 먼저·넣은 순서대로 일반 첨부와 **같은** 동시 내려받기 한도(`blobs::borrow_slot`, Tor로 도는 기기면 1)에서 자리를 빌려 시도를
    시작합니다. 자리가 없으면 기다리고, 그동안 일반 대기열은 빈자리 하나를 남겨 둡니다(휘발성 첨부는 곧 사라지므로 먼저). 시도
    하나는 메시지가 살아 있는지 보고 참가자에게 매니페스트를 묻고(`p2p::request`, P2P 전용 채널 참가자는 P2P 스웜에 있음. 메시지에
    적힌 크기 + 64KiB·하나 한도를 넘으면 포기, 하나 한도는 자동이면 `channel_max`, 직접 받기는 이 기기 설정만)
    `register_blob`으로 RAM 예산을 크기만큼 잡은 뒤 일반 첨부와 같은 스웜(`ChunkScheduler`, 조각 검증, 틀린 조각 감점, 같은 수신
    대역폭 버킷 `blobs::with_download_bucket`, 겹침은 일반 첨부처럼 가진 참가자(모르면 모든 참가자)의 연결로 `blobs::Pacer`가 정하고
    가진 참가자가 바뀌면 다시 정함)으로 받습니다. 15초 진전이 없으면 시도를 끝내고 다시
    대기열로(1초부터 두 배, 최대 10초). 다시 시도를 기다리는 동안은 예산을 붙잡지 않습니다: 조각을 하나도 못 받았으면 지우고,
    받다 만 것은 가진 바이트로 줄이며(`trim_blob_reservation`) 다음 시도에서 다시 전체를 잡습니다(모자라면 그만둠). 10분 동안
    아무것도 못 받거나 그만두면 대기열에서 빼고 받다 만 조각도 지웁니다(`blob.fetch`로 처음부터 다시). 메시지가 지워지면
    저장소가 블롭을 지우므로 받던 시도도 멈추고, 채널을 나가면(`wipe`) 그 채널의 대기 요청을 버립니다.
  - 내주기(`attach::serve`): 인증·멤버 기기 → 그 채널에 맞는 길(`p2p::route_ok`) → 휘발성 채널 → 블롭이 메시지에 묶여 있고 그 메시지가 보기에 있음 → 요청 기기가 지금
    참가자(`author_present`) → `sync_service::check`(채널은 연 것으로, 데이터 종류는 첨부 MIME으로). 그 밖에는 `NotFound`.
  - 지우기: 블롭은 저장소에서 주인 메시지와 함께 지워집니다(`remove_event`·`drop_channel`·`expire`). 색인은 `remove_events`가 정리.
  - 보기: `build_view`가 본문을 풀 때 첨부 보기를 만들고(`downloaded` = RAM에 다 있음) 이름·썸네일을 덮어씁니다.
  - 로컬 API: `api/blob.rs`가 `attach::find`로 휘발성 첨부인지 먼저 봅니다. `blob.status`·`fetch`(한도 `1005`·예산 `1010`)·`read`
    (첨부 키는 그때마다 본문을 풀어 꺼냄)·`save`(사람 토큰 + `allow_volatile`, 아니면 `1002`·`1010`). 자동화 토큰은 API 읽기 허용 전까지 `1007`.
- **로컬 API**: `message.send`(게시글 댓글은 `1005`, 첨부는 위, 링크 미리보기는 채널 설정과 상관없이 만들지 않음), `message.list`(참가 중이 아니면 빈 목록),
  `edit`·`delete`·`react`·`unreact`. 자동화 토큰은 `channel.allow_api_read` 전까지 `message.list`가 `1007`이고 알림은 내용 없이
  (`default_notification_filter`), 허용하면 사람 토큰처럼 받습니다. `channel.mark_read`의 읽음 위치는 RAM에만.
  `channel.list`의 `joined`·`participants`·`unread`·`api_read_allowed`는 `fill_channel_view`가 채웁니다.

## 진단 (`diag`)

- `diag.metrics`: 전달 지연 p50·p95(피어가 보낸 메시지 레인 이벤트의 `받아들인 시각 − 작성 시각`, 최근 1024개, 5분에서 자름),
  초당 받아들인 이벤트(지난 10초 평균), 프로세스 메모리(RSS)·CPU(지난 호출 뒤 누적 CPU 시간 차이, 첫 호출은 0, `sysinfo`),
  `queues`: `ingest_held`(RAM 보류)·`ingest_waiting`(키·대상·상태 대기, 적용 중인 공간은 건너뜀),
  `volatile_channels`·`volatile_events`·`volatile_event_bytes`·`volatile_keys`·`volatile_blobs`·`volatile_blob_bytes`·
  `volatile_blob_reserved`. 숫자만 다룹니다.
- `diag.leakcheck({marker})`: `spawn_blocking`에서 `Store::scan_for_marker`(복호화한 DB의 모든 열, 저장 키로 봉인된 열을 풀어서,
  데이터 디렉터리의 모든 파일. SQLite `-shm`의 잠금 바이트 120..136은 Windows에서 다른 연결이 잠근 동안 읽을 수 없어 건너뛰고,
  바이트 범위 잠금에 걸린 읽기는 잠깐 뒤 다시 시도)와, 데이터 디렉터리 밖에 둔 `--log-file`을 훑습니다. 표식은 `Zeroizing` 버퍼로만 다루고 어디에도
  쓰지 않으며(감사 로그도 메서드·결과만), 결과는 위치(`db:<표>.<열>`, `db:<열>(복호화)`, `file:<상대 경로>`, `log:<경로>`)만 담습니다.
  첨부도 봅니다: 키를 아는 디스크 블롭(첨부를 실은 메시지·공간 이모지·아이콘·프로필 이미지)은 `Store::scan_blobs_for_marker`로
  조각마다 풀어 평문을 훑고(`blob:<앞부분>`, 조각 경계에 걸친 표식 포함), 지금 RAM에 있는 휘발성 첨부 ID가 디스크 블롭 저장소에도
  있으면 `blob-store:<앞부분>(휘발성 첨부)`. 남의 블롭을 시더로만 들고 있어 키를 모르는 것은 풀지 못합니다.

## 크루 (B1, `crews`)

- 초대 가입(가입자): `invite.join` → 대기 기록(설정 `crew.join.<공간>`: 시작 이벤트, 받아 준 노드, 만든 사용자) → 링크 접속
  정보의 노드에 `RedeemInvite` → 받은 공간 스트림 묶음 검사(이 공간의 공간 스트림만, 링크의 시작 이벤트가 서명이 맞는
  `SpaceCreate`) → 시작 이벤트 고정·적용. 멤버이고 크루 키(가입 갱신)를 얻었으면(`keygroup::has_crew_key`) `joined`, 아니면 `pending`이고 감시 작업이
  1초~30초 간격으로 받아 준 노드(상태를 풀었으면 다른 인증된 멤버 노드도)에서 따라잡습니다. 대기 중에는 받아 준 노드와 공간을
  만든 사용자의 기기만 인증·동기화 상대로 믿습니다. 재시작하면 `crews::start`가 이어 갑니다.
- 초대 가입(받는 쪽, `handle_redeem_invite`): 인증·프로필 확인 → 토큰 해시·만료·횟수, 초대 증명(`InviteProof`)을
  초대의 검증 공개키로 확인 → `MemberJoin{invite: 증명}` 발행 → 내가 확정 기기면 묶음 창 뒤 키 그룹 바퀴(`keygroup::reconcile_batched`,
  동시에 온 가입은 `Advance` 하나와 가입자마다 받은편지함 묶음) → 공간 스트림과 가입자 받은편지함을 `InviteAccepted`로(`bootstrap_bundle`: 시작 이벤트 먼저,
  오래된 것부터 512개·어림 4MiB 안, 받은편지함은 그중 1/4까지) → 가입자와 인증. 가입자(`join`)는 토큰 키쌍으로 (공간, 토큰 해시, 시각, 내 기기 목록)에 서명하고, 같은 내용에
  기기 키로도 서명해(`InviteProof::joiner`) 보냅니다. 토큰은 상태에 실리지 않습니다.
- 비공개 키 그룹: 볼 수 없는 멤버가 하나라도 있는 채널(`needs_channel_group`, 역할 기본 권한·멤버별 거부 포함)은 그 열람 규칙의
  키 그룹(`ChannelSettings::key_group`)을 씁니다(확정 기기 쪽 `committer::desired_groups`, 보내는·받는 쪽 `keygroup::channel_group`).
  휘발성·카테고리 채널은 없음. 포럼 제목·태그 변경 거절도 같은 판단을 씁니다.
  `on_state_applied`는 새로 볼 수 있게 된 채널이 생기면 인증된 멤버 피어와 바로 따라잡습니다(역할을 받은 직후 지난 기록).
- 탈퇴(`crew.leave`): `MemberLeave` 발행 → 인증된 멤버 노드에 바로 `PushEvents`(`NotMember` 답도 받은 것으로 봄), 하나도 못
  받으면 `1009`로 끝내고 데이터를 남김 → 받았으면 `purge_local`(가입 대기·포럼 설정, `spaces::discard_space_locked`).
  추방된 뒤에 부르면 저장된 내 탈퇴가 있으면 다시 보내 보고 지웁니다.
- 추방(`member.kick`): 발행 후 쫓겨난 사용자의 인증된 기기에도 추방 이벤트를 직접 보냄(`tell_removed`). 그 노드는 멤버가 아님을
  보고 쓰기 API가 `1003`이 되며, 받은 기록은 `crew.leave` 때까지 남습니다.
- 포럼 게시글 변경: 해소기에 넣기 직전 공간마다 그 ID만 RAM에 기억(`on_decoded`, 재시작하면 되풀이 적용으로 다시 쌓임) →
  해소 결과(`on_resolved`)에서 그 ID를 골라, 새로 적용된 것은 저장된 이벤트를 상태 키로 다시 풀어 게시글마다 설정 한 줄
  `forum.meta.<공간>.<게시글>`(내용 없이 ID·논리 시계, 변경 256개 상한)에 더하고 항목별 마지막 쓰기 우선으로 반영, 다시
  해소하며 빠진 것(`unapplied`)은 그 줄에서 빼고 남은 변경으로 다시 계산(`recompute_post`)합니다. 그래서 먼저 거부됐다가
  나중에 적용되는 변경도 잃지 않습니다. 게시글을 아직 모르면 그 줄에만 두었다가 게시글이 오면(`on_forum_post_applied`)
  계산합니다. 남의 게시글 제목·태그 변경은 적용할 때 그 사람에게 `MANAGE_MESSAGES`가 있어야 합니다(해소기는 게시글 작성자를 모름).
- 소유권 이전(`transfer_ownership`): `OwnershipTransfer`만 발행합니다. 이전 뒤 확정 기기가 새 소유자 기기로 옮겨 가면(해소기
  `repair_committer`) 옛 확정 기기가 표를 넘기고, 못 넘기면 새 확정 기기가 공개 끝으로 이어받아 전체 재키합니다(위 "커밋 확정
  노드"). 확정 기기는 표만 있으면 어느 그룹이든 갱신을 낼 수 있어 새 소유자를 그룹마다 먼저 넣을 일이 없습니다.
- 탈퇴·1:1 전환(`crew.leave`, `crew.convert_request`, `crew.convert_accept`)은 파괴적 메서드라 자동화 토큰이면 사람 확인.
  `channel.update`가 과거 기록을 공개로 바꾸면 자동화 토큰은 사람 확인(`admin::confirm_when_others`).

## API와 안전장치 (`api`, `admin`)

- `ApiHandler`가 `yangpa_rpc::RpcHandler`를 구현합니다. 토큰 인증: 소유자 토큰(해시를 상수 시간 비교) 또는
  `api_tokens`의 API 토큰(`blake3::derive_key("yangpa/v1/api-token", 토큰)` 해시). 토큰 원문은 `token.create` 결과에만 나옵니다.
- 호출 순서: 종료 중이면 거절(`system.shutdown` 제외) → `admin::before_call` → 메서드 → `admin::after_call`.
- `before_call`: 폐기한 토큰이면 `1001`(서버 연결은 인증 정보를 연결할 때 받아 두므로, 폐기 목록을 노드가 들고 이미 붙은
  연결의 다음 호출도 막음). 자동화 토큰이 항상 파괴적인 메서드(`destructive`)를 부르면 실행하지 않고 확인 요청을 저장해
  `confirm.pending`을 알리고 `1006`(`data.confirmation_id`)을 돌려줍니다. 대상에 따라 파괴적인 메서드(`message.delete`의 남의
  메시지)는 메서드가 판단한 뒤 `admin::confirm_when_others`를 부릅니다.
- 확인 요청: `{ID(무작위 16바이트 base58), 요청 토큰 ID, 메서드, 인자, 한국어 요약, 만료}`. 요약에는 메서드 이름과 대상·채널·
  역할·공간 이름만 넣고 메시지 내용은 넣지 않습니다. 기본 1시간 뒤 만료(1분마다, 그리고 목록·승인 때 정리), 대기열 100개.
- `confirm.approve`: 사람 토큰만. 승인하는 토큰도 원래 메서드의 범위·공간을 다룰 수 있어야 합니다. 요청을 원자적으로 꺼내
  (동시에 두 번 승인해도 한 번만 실행) 요청한 토큰의 지금 범위·공간 제한으로 다시 검사한 뒤 `api::dispatch`로 실행합니다
  (사람이 승인했으므로 자동화 표시는 끔). 감사 로그에는 원래 토큰 이름으로 남깁니다. 토큰을 폐기하면 그 토큰의 요청도 지웁니다.
- 확인 요청은 저장소(`confirmations`)에 두되, 대상이 휘발성 채널이면 `AdminState.volatile_pending`(RAM)에만 둡니다(목록·승인·
  거절·만료·토큰 폐기는 두 곳을 함께 봄, 재시작하면 사라짐).
- 감사 로그: 인증한 모든 호출(확인 대기·폐기 토큰 거절·범위 거절, 서버가 답한 `auth.hello`·구독 포함)과 인증하지 않은
  `auth.hello` 실패의 메서드, 토큰 이름, 성공 여부·오류 코드, 자동화 여부, 시각. 인자·결과는 남기지 않습니다. 줄마다
  `important`(실패, 승인 실행, 파괴적·사람 전용 메서드, admin·owner 범위, `token.*`·`confirm.*`; 토큰 없는 줄은 제외)를 매겨
  등급별로 따로 보관합니다(평범한 줄 1만, 중요한 줄 5만, `NodeConfig::audit_caps`).

## 기기·프로필·메모 (B4)

- 기기 목록(`identity.rs` "기기 목록·프로필 전파"): 목록은 신원 서명에 판을 만든 기기의 공동 서명(`DeviceCosig`)을 답니다.
  `accept_device_list`가 모든 경로(`Auth`, `Social(Devices)`, 상태 이벤트 본문, `Revoked` 답)에서 받은 목록을
  `SignedDeviceList::classify`로 아는 목록에 비춰 봅니다(처음 보는 사용자의 목록은 서명만 봄). 남의 목록이 저장된 판보다 높지
  않으면 서명을 보기 전에 받지 않습니다(어차피 이어받지 못함: 시작 때 다시 세우며 가입·기기 목록 갱신마다 지난 판을 다시 볼 때
  판마다 혼합 서명을 확인하지 않게):
  - `Authorized`(아는 목록에서 활성인 기기가 만든 판): 남의 목록은 이어받을 때 저장, 내 목록은 이어받거나 합쳐
    (`DeviceList::merge`, 합친 판은 내 공동 서명) 채택하고 바뀌면 `announce`(내가 쓸 수 있는 공간마다 `DeviceListUpdate` +
    인증된 피어에게 `Social(Devices)`). 그 판이 이 기기를 폐기했으면 로컬 데이터를 지웁니다(아래).
  - `Recovery(기기)`·`OtpMissing(기기)`(옛 빌드의 복구 판: 아는 목록에 없던 기기가 자기만 더함): 받지 않습니다. 내 목록이면
    버리고, 남의 목록은 공동 서명 기기를 몰라 다른 판처럼 미뤄 둘 뿐입니다(`defer_list`, 다음 판을 받으면 다시 판정). 살아 있는 기기의
    승인이 없는 판이기 때문입니다. 이 판정은 크루 해소기가 옛 기록을 다시 적용할 때 쓰는 규칙이라 남아 있습니다(decisions.md "기기 연결").
  - `Rejected`(공동 서명 없음, 아는 목록에서 폐기된 기기가 만듦, 아는 목록을 이어받지 않거나 아는 기기를 폐기하거나
    다른 기기까지 더함): 버립니다. 내 목록이고 내 목록에서 폐기된 기기가 기기를 더한 판이면, 내 활성 기기가 내 목록 그대로에
    그 판이 더한 기기만 폐기로 덧붙인 판(`DeviceList::heal`, 위조 판보다 높은 판)을 내 공동 서명으로 만들어 퍼뜨립니다(바로잡기:
    폐기를 아직 모르고 그 판을 먼저 받은 노드도 그 기기들을 폐기로 보게). 위조 판의 폐기·상자 키는 가져오지 않습니다.
- 시작할 때 `identity::forget_old_otp`가 없어진 OTP가 남긴 비밀(`otp.mine`, `otp.escrow.<사용자>`)을 지웁니다(decisions.md "OTP 제거").
  목록의 OTP 키(`DeviceList.otp`)는 서명 대상이라 그대로 이어받습니다.
- 폐기된 기기 지우기(`identity::wipe`): 이 기기를 폐기한 내 목록(내 활성 기기가 공동 서명)을 받거나(인증 답 `Revoked`, 내 목록
  전달), 다른 사용자의 노드가 `Revoked`로 알려 준 판이 내 목록을 이어받고 공동 서명 기기를 내가 모르면(`vouched_revocation`: 내가
  오프라인인 사이 더해진 기기가 폐기함) 적용 잠금을 쥔 뒤 신원·기기 키·네트워크 키·공간·메시지·블롭·설정을
  모두 지우고 `device.wiped`를 알립니다. 휘발성 상태(`volatile::wipe_all`)와 모더레이션의 RAM 사본(`moderation::wipe_all`: 차단
  목록·숨김·휘발성 신고)도 비워, 같은 프로세스에서 새 신원을 만들어도 옛 기록이 이어지지 않습니다. 네트워크 설정(`net.*`: STUN 서버 목록)만 남겨, 다시 시작해도 고른
  설정이 풀리지 않게 합니다(P2P 참여 기록 `p2p.joined`는 지워짐). 저장소의 `Store::wipe_all(keep_secrets, keep_settings)`가 지우기와 남기기를 한 트랜잭션으로
  하므로 중간에 실패해도 네트워크 설정을 잃지 않고, 블롭 디렉터리를 다 지우지 못하면 `store.blob_wipe_pending`을 남겨 다음 시작
  때 다시 지웁니다. 네트워크 키는 지우므로 다시 시작하면 새 PeerId를 씁니다(폐기된 신원과 이어지지 않게). onion 서비스
  키도 지워(`NetHandle::forget_onion_keys`) 다음에 띄우면 새 onion 주소가 됩니다.
- 남의 새 목록을 받은 노드도 함께 있는 공간에 대신 넣습니다(`relay_list`: 내가 그 공간에 쓸 수 있고, 그 사용자가 멤버이고,
  상태의 목록을 이어받고 그 목록에서 활성인 기기가 공동 서명했을 때만). 새 기기는 확정 기기의 다음 크루 갱신(`Advance`)으로 키 그룹에 들어갑니다(상자
  키는 목록에 실려 있어 따로 받을 것이 없음).
- 폐기(`identity::revoke_device`): 신원 잠금 안에서 새 판을 서명(내 공동 서명)·저장하고 `me`를 바꾼 뒤, 그 기기의 인증을 잊고
  (`PeerDirectory::forget_devices`) 모든 공간에 전달합니다. 나머지(키 그룹에서 빼는 `Rotate`, 묶음 창으로 다른 제거와 묶음)는
  커밋 확정 노드의 바퀴이고, 폐기된 기기가 확정 기기였으면 위 "커밋 확정 노드"의 표 넘기기·공개 끝 이어받기가 잇습니다. 폐기할
  기기는 지금 내 목록에 있어야 합니다(없으면 `not_found`).
- 받은 기기 목록은 서명 확인 전에 모양(항목 64개 이하, 기기 중복 없음: `DeviceList::well_formed`)부터 봅니다. 내 목록을 합친
  결과가 상한을 넘으면 채택하지 않습니다(목록은 늘기만 하므로 평생 기기 64대). 폐기한 목록을 받지 않은 공간이 있으면 경고 로그.
- 새 기기 연결(`devlink.rs`, protocol.md "새 기기 연결"): 상태 `DeviceLinkState`는 RAM에만 둡니다(`offer` 띄운 링크 하나, `granted`
  승인해 새 기기가 받아 가기를 기다리는 묶음, `joining` 이 새 기기가 기다리는 토큰).
  - 쓰던 기기: `create`가 토큰을 만들어 링크와 QR을 돌려줍니다(10분, 앞서 띄운 링크는 끝남). 링크가 사는 동안(`wants_direct`) 기기가 하나여도
    직접 스웜을 띄우고 `p2p::link_contact`가 그 노드와 주소를 링크에 싣습니다. `handle_request`(`YpRequest::DeviceLink`,
    인증 전)는 기기 항목을 보고(키가 기기 ID와 맞음, 폐기 표시 없음, 이름 100자 이하) 토큰이 맞으면 논스를 만들어 번호를 세고
    (`device_link_code`) `device.link_request`를 알립니다. 같은 기기가 다시 청하면 같은 논스를 주고, 다른 기기가 또 쓰면 `contested`를 켜
    다시 알린 뒤 `Forbidden`으로 거절합니다. 거절한 링크는 `Expired`입니다.
  - `decide`: 승인이면 `Approving`으로 둬서 그동안 `create`·`cancel`이 링크를 끝내지 못하게 하고, `seal_grant`가 `identity::add_device`로
    내 목록에 넣은 뒤 `DeviceLinkGrant{seed, devices, profile}`를 새 기기의 상자 키로 봉인합니다. 링크는 끝나고 묶음은 `granted`에
    10분 남습니다(답이 끊겨도 다시 물으면 같은 것을 주고, 새 링크를 띄워도 남음). `handle_poll`은 요청을 보낸 그 노드에게만 답합니다.
  - 새 기기: `join`이 기기 키를 만들고 `reach`로 링크의 직접 주소(`p2p::dial_link`)와 Tor(`friends::reach_peer`)를 함께 걸어, 먼저 닿은 길(`Route`)로
    요청을 보낸 뒤 번호를 돌려줍니다. 직접으로 닿아도 Tor는 뒤에서 이어 겁니다. 뒤에서 `wait_grant`가 그 길로
    1초마다 `DeviceLinkPoll`로 묻습니다(10분, 끊기면 다시 겸). 묶음을 받으면 `finish`가 봉인을 열어 시드·목록·프로필이 링크의 사용자와 맞고
    이 기기가 활성인지 본 뒤 `identity::install_linked`로 설치합니다. 이어서 `p2p::reconcile`로 내 기기끼리 직접 잇고
    `identity::spawn_first_sync`를 띄우고 `first_auth`가 승인한 기기와 Tor로 인증해(`peers::ensure_auth`, 5번까지) 따라잡습니다. 결과는 `device.link_result`입니다.
    `abort`는 `joining`을 비워 묻기를 멈춥니다(결과 알림 없음).
  - 첫 동기화(`identity::spawn_first_sync`): 내 기기가 공간을 알려 주고 받아들인 공간이 모두 상태와 크루 키를 얻고 따라잡기가 없는 때가
    5초 이어지면 `device.synced`를 알립니다. 따라잡기 없이 45초가 지나도 끝냅니다(받을 곳이 사라진 경우). 상태는 RAM에만 둬서 다시 켜면
    주기 동기화가 화면 없이 이어 받습니다.
  - 공간: 내 기기의 `Social(Devices)` 요청·답에 실린 모든 공간을 받아들입니다(`take_own_state` → `adopt_space`): `identity.adopt.<공간>`에
    `{시작 이벤트, 시각, 연락처 기록?}`을 남기고 런타임을 만든 뒤 상태와 크루 키가 생길 때까지 따라잡기를 몇 번 더 돌립니다
    (250ms부터 두 배, 최대 5초, 40번). 그 뒤와 재시작 뒤에는 `catch_up_peer`(받아들이는 중인 공간도 봄)와 주기 동기화가 잇습니다.
  - 상태가 처음 생기면 표시를 지우고, 1:1 공간이면 친구 기록을 되살립니다(`restore_friend`, 함께 받은 연락처 기록의 종류·조건대로). 30일 안에 해소되지 않으면 포기합니다(`adoption_tick`).
  - 연결된 뒤로는 `spawn_own_share`가 연락처 기록·받은 친구 추가 요청·내 새 공간이 바뀔 때마다 인증된 내 기기에 같은 내용을 보냅니다(protocol.md "내 다른 기기로 전하기").
- 프로필: `on_authenticated`의 `exchange_profile`, `profile.set` 뒤 `push_profile`(인증된 피어 모두 + 셋 이상인 공간에
  `MemberProfile{profile}` 상태 이벤트). 받은 프로필은 서명·판 확인 후 저장하고, 내 프로필(다른 기기에서 바꿈)이면 `me`도
  바꿉니다. 저장한 판이 바뀌었으면 `blobs::on_profile_changed`(위 "주기 작업")를 부르고 `profile.updated {user}`를 알립니다. `profile.set`도
  `push_profile` 뒤 내 사용자로 `profile.updated`를 알려(상태 이벤트를 내지 않는 1대1·작은 공간에서도) 내 화면의 이름이 바로 바뀝니다. 크루 별명은 `profile.set_crew` → `MemberProfile{nickname}`(역할을 싣지 않음).
- 메모(`notes.rs`): 저장소 `notes` 표 + 메모마다 `note.meta.<사용자>` 설정(`{updated_at}`, 지움도 남김). 쓰기는 내 인증된 기기에
  하나씩, 내 기기와 인증되면 전부를 4MiB·4096개 이하의 묶음 여러 개로 나눠 차례로 봉인해 보내고(마지막 묶음에 `full`), 받은 쪽은
  자기 것 전부를 같은 방식으로 따로 보냅니다(잘라 버리는 메모 없음, protocol.md "개인 메모").
- 친구 삭제(`friends.rs` "친구 삭제"): `friend.leaving.<사용자>`에 `{공간}`을 남기고 `friends::cut`(묘비 `friend.cut.<공간>`, 연락처·요청·신뢰
  삭제, `discard_space_locked`, `identity::forget_stranger`, `friend.updated`·`space.updated`) 뒤 지금 인증된 상대에게 `Social(Cut)`. 상대가 받거나(`Ok`)
  이미 공간을 모르거나 나를 멤버로 보지 않으면(`NotMember`·`NotFound`) 지웁니다. 남은 것은 다음 인증 때 건 쪽(`on_authenticated`)이든 받은
  쪽(`on_auth_accepted`)이든 다시 전하고, 상대가 내 인증을 `NotMember`로 거절하거나 30일이 지나면 지웁니다(그동안만 그 사람의 `Auth`를 받아 줌).
  받는 쪽은 `friends::handle_cut`, 내 다른 기기는 `Social(Devices)`의 `cut`을 `take_own_state`가 `friends::take_cut`으로 지웁니다.
  `friends::on_state_applied`는 1:1 공간에서 멤버가 나간 상태를 만나면, `friends::spawn_cut_sweep`은 시작할 때 한 사람만 남은 1:1 공간을 같게 지웁니다.

## 모더레이션 (W2, `moderation`)

- 밴은 상태 이벤트(`MemberBan`·`MemberUnban`)라 해소기(`yangpa-sync`)가 권한·순위·재가입 거부를 정하고, 기기 제거·상태 키
  회전은 추방과 같은 길(확정 기기)로 갑니다. `api/moderation.rs`는 미리 검사하고 이벤트를 내며 멤버였으면 `crews::tell_removed`로
  그 기기들에 알립니다. 초대를 받아 주는 노드는 밴된 사용자의 `RedeemInvite`를 `Forbidden`으로 거절합니다(`crews.rs`).
  `delete_recent`의 지우기는 `moderation::on_resolved`가 밴이 적용된 노드마다 `Store::messages_by_author_between`(구간 끝 = 밴
  시각) → `mark_message_deleted`로 하고(삭제 이벤트를 내지 않음), 끝내면 `mod.bandel.<공간>.<밴 이벤트>`를 남겨 시작 때 상태를
  다시 세우며 되풀이하지 않습니다.
- 차단: 설정 `mod.blocked.<사용자>`(`{blocked_at}`)와 RAM 사본(`ModerationState::blocked`, 알림 거르개가 같은 지도를 봄).
  처음 쓸 때 설정에서 읽어 옵니다. 차단할 때 그 사용자와 오가던 친구 요청을 지웁니다.
- 숨김(신고하며 `hide`): 일반 등급은 설정 `mod.hidden.<공간>.<이벤트>`와 시더 정책의 개별 메시지 제외(`policy::set_excluded`),
  휘발성 채널은 RAM에만.
- 신고:
  - 보내는 쪽: 같은 메시지를 다시 신고하면 앞 ID(`mod.sent.<공간>.<메시지>`). 받는 기기마다 봉인한 묶음을 대기열
    `mod.outbox.<무작위 ID>`(봉인돼 있어 이 기기도 다시 열 수 없음, 채널 ID를 함께 둠)에 넣고 바로 한 번 보낸 뒤, 5초마다 이미
    인증된 연결로 다시 보냅니다(새로 넣을 때만 주소 기록으로 연결을 엶). 받는 쪽 답이 `BadRequest`일 때만 버리고
    (`moderation::keep_after`), 나머지 거절과 `Busy`는 받는 쪽 상태가 따라올 때까지 다시 보냅니다.
  - 받는 쪽(운영진): `mod.report.<공간>.<신고 ID>`에 `{신고 ID, 채널, 메시지, 신고한 사람, 이유, 시각, 처리 결과·시각·처리한 사람}`.
    새 신고면 `report.new`. 열린 신고 상한(공간 1천, 신고한 사람 50)을 넘으면 `Busy`, 처리한 신고는 공간마다 1천 건만 남김
    (`prune_resolved`).
  - 휘발성 채널 메시지의 신고는 보낸 기록·대기열·받은 기록 모두 `ModerationState`의 RAM 지도에만 둡니다.
  - 처리: `report.resolve`는 `dismiss`가 아니면 이 기기의 메시지 사본으로 작성자를 확인하고(휘발성은 `volatile::author_for_api`로
    RAM의 메시지 하나를 바로 찾음) `admin::confirm_when_others`(자동화 토큰이면 확인 대기열, 인자에 신고된 채널을 넣어 휘발성
    채널 신고면 RAM 대기열로)를 거친 뒤 `message.delete`·`member.kick`·`member.ban` 구현을 그대로 부릅니다. 결과는
    `mark_resolved`가 기록하고 다른 운영진에게 보냅니다.
- 로그에는 신고 이유·메모·메시지 내용을 남기지 않습니다(전달 실패의 오류 코드만 `debug`).

## 상태 체크포인트 (`checkpoint`)

설계는 protocol.md "상태 체크포인트". 상태는 `Node::checkpoints`(`CheckpointState`), 설정은 `NodeConfig::checkpoint`
(`CheckpointOptions`: 문턱 256대·5,000개, 그 뒤 1,000개·24시간, 가라앉음 5분, 합치기 48시간, 확인 간격 60초, 두 번째 출처 기다림 1분).

- 내기(`tick` → `consider` → `issue`): 크루마다 확인 간격으로 봅니다. 쓰는 기기(위임받은 기기, 아니면 소유자의 확정 기기나 가장
  작은 소유자 기기)만 내고, 다른 소유자 기기는 합치기 시간의 두 배를 기다립니다. 공간 잠금 안에서 해소기 초안을 만들고, 잠금 밖
  (blocking 작업)에서 스냅숏을 암호화·저장(고정)한 뒤, 다시 잠금을 쥐고 바탕이 그대로인지 본 다음 본문의 머리를 부모로
  `author::publish_with_parents_locked`로 냅니다. 바탕이 바뀌었으면 만든 블롭을 지우고 그만둡니다.
- 받은 뒤(`on_accepted`): 바탕 직후 상태(`Resolver::base_snapshot`)로 블롭을 다시 만들어 ID가 같으면 저장·고정·시드(`blobs`의
  `Use::Snapshot`, 내주기는 대역폭 검사만), 다르면 이견. 사슬 본문(`checkpoint.chain.<공간>.<순번>`, 최근 128개)과 바탕 기록
  (`checkpoint.base.<공간>`: 고정, 이벤트, 경계, 스냅숏, 상태 키 ID, 남긴 블롭 2개)을 두고, 경계 아래 빠진 부모 기록을 지웁니다.
- 경계(`floors`): 해소기 바탕의 F, 가입 중이면 받아 둔 체크포인트의 F와 큰 것. 잠금 없이 읽습니다(`author`·`sync_service`).
  따라잡다 받은 F 아래 (ID, 논리 시계)는 `below`(공간마다 4,096개)에 기억해 상대 머리에 남은 늦은 이벤트를 매번 다시 받지
  않습니다(`note_below`). 해소기는 F 아래 부모를 판정에 쓰지 않지만 모르면 빠진 부모로 기다리므로, `ingest`가 넣기 전에 묶음
  항목의 F 아래 부모(저장소나 `below`에 있는 것)를, 넣은 뒤에는 해소기가 기다리는 것(`below_waited`)을 부모 없는 순서 연결로
  알립니다. 저장소에서 다시 넣을 때 F 아래 이벤트는 키를 기다리지 않고 순서에만 씁니다(은퇴한 키는 새 멤버에게 오지 않음).
- 부모 고르기: 공간 스트림 이벤트의 부모는 해소기 머리(`resolver_heads`, F 이상)입니다. 저장소 머리는 F 아래 자식(낮은 논리 시계로
  지어낸 것 포함)이 F 위 진짜 머리를 가릴 수 있고, 그것을 덮지 않으면 체크포인트를 영영 낼 수 없습니다. 소유권 이전 인증서는
  지금 바탕(`base_id`)에 묶습니다.
- 가입(`crews::join`): 묶음을 적용하기 전 `prepare_join`(받아 준 노드와 인증 → `GetCheckpoint` → 사슬 확인(링크의 고정에서) →
  받아 둠(`checkpoint.stash.<공간>`) → 경계 → 스냅숏 조각 받기). 실패하면 `prepare_later`로 감시
  작업이 다시 묻습니다(그동안은 처음부터 따라잡음). 상태 키가 오면 `try_install`이 스냅숏으로 해소기를 세우고 F 이상 이벤트를 다시
  넣습니다(가입·감시 작업이 함께 불러도 잠금 안에서 받아 둔 그대로인지 다시 봄). 확인에 실패하거나 10분 동안 스냅숏을 받지 못하면
  받아 둔 것을 버리고 경계를 해소기 것으로 되돌립니다. 고정이 없는 링크는 체크포인트를 쓰지 않고 처음부터 받습니다.
- 두 번째 출처(`confirm`): 체크포인트로 설치한 공간은 확인 전(`unconfirmed`, 설정 `checkpoint.unconfirmed.<공간>`에 저장해
  재시작 뒤 지난 시간까지 이어 셈)이고, 그동안 `author`가 그 공간의 쓰기를 `Conflict`로 막습니다. 감시 작업이 확인 간격마다
  받아 준 노드가 아닌 피어 4개까지(인증된 피어 먼저, 이어졌지만 인증하지 않은 피어는 `ensure_auth` 뒤)에 `GetCheckpoint`를
  물어 `compare_heads`로 견줍니다. 같거나 이어지는 답이 있으면 확인을
  마치고, 갈라진 답이 있거나 `second_source_wait`를 넘기면 `fall_back`: 해소기를 시작 이벤트로 되돌리고 바탕 기록·사슬·경계·
  `below`를 지운 뒤 저장된 공간 스트림 이벤트를 다시 넣고 모든 피어와 따라잡으며 `checkpoint.alert`(`fallback`)를 냅니다.
- 경보(`alert`): 해소기가 알린 겨룬 체크포인트(`ResolveUpdate::conflicts`)마다 `ingest`가 경고를 남기고 `checkpoint.alert`
  (`equivocation`이나 `conflict`)를 냅니다.
- 답하기(`inbound`): `GetCheckpoint`(멤버 기기만, 남긴 사슬·최신 이벤트·매니페스트), `GetDeviceKeys`(멤버 기기만, 512개까지,
  바탕 기기 트리 증명표는 바탕마다 한 번 만들어 둠). 받은 기기 키는 `check_device_keys`(묻은 기기, 키 약속이나 증명)로 거른 뒤
  기기 키 표에 넣습니다. 확정 기기는 체크포인트가 있을 때 맞출 구성원 가운데 키가 없는 기기의 키를 미리 받습니다
  (`committer` → `prefetch_device_keys`).
- 다시 서명(`resign`): 내 상태 이벤트가 `LateEvent`·`RetiredStateKey`로 거부되면 같은 본문을 지금 머리·지금 상태 키로 한 번
  다시 냅니다(체크포인트·이견 이벤트는 빼고). 저장소에서 다시 넣은 판정(`Source::Replay`)으로는 내지 않고, 다시 낸 이벤트 ID를
  내기 전에 `checkpoint.resigned.<공간>`(최근 1,024개)에 저장해 재시작해도 또 내지 않으며, 7일보다 오래된 이벤트는 내지 않습니다.
- 상태 키: 은퇴한 상태 키는 새 멤버에게 봉인하지 않습니다(`keys::all_state_keys`, 지금 키는 늘 넣음).
- 로그에는 공간 ID 앞부분·순번·오류만 남기고 스냅숏 내용·키는 남기지 않습니다.

## 보관 전용 노드 (X3, `archive`)

설계는 protocol.md "보관 전용 노드". 한 데몬이 두 역할을 다 할 수 있고 상태는 `ArchiveState{host, member, wake}`입니다.

- 멤버 쪽(`archive/member.rs`): `archive.designate`는 링크의 노드에 연결해 첫 명단과 `Designate`를 보내고, 받아들이면
  `ArchiveGrant`를 발행합니다. `space.updated`마다와 주기 작업(`sync_interval`, 최대 60초)마다 지정된 보관 노드에 명단을 보내고
  (권한자는 내용이 바뀌었을 때, 밴·추방 권한자는 내용이 바뀌었을 때, 그 밖의 멤버는 자기 항목이 바뀌었을 때. 보내기 전에
  `sync_service::sync_space_stream`으로 보관 노드와 공간 스트림을 맞추고, `Expired`면 기록하지 않고 물러났다 다시 보냄. 주기적으로
  통째 명단을 다시 보내지 않음. 바탕 체크포인트가 있으면 권한자는 바탕 명단을 한 번 보내고 그 위의 차이(`RosterDelta`)만 보내며
  사슬을 `archive.delta.<공간>.<보관 기기>`에 둠. 가입을 받아 준 멤버는 가입자를 넣는 차이를 바로 보냄(`add_joiner`)),
  보관 범위 메시지의 첨부를 알리고(`BlobRefs`), 지워진 메시지를 알리고(`Prune`, 설정 `archive.prune.*`에 남겨 다시 보냄), 상태에서
  사라진 지정에 해제를 알립니다(`archive.release.*`). 마지막으로 본 지정 목록은 `archive.known.*`에 둡니다. 키 그룹 스트림은
  보관 노드에 주지 않습니다.
- 보관 노드 쪽(`archive/host.rs`): 제안(`archive.offer.*`, 해시·만료만), 받아 둔 공간 기록(`archive.hosted.*`: 명단·소유자 서명
  기기·사용량·지우기를 기다리는 빠진 채널), 첨부 쓰임(`archive.ref.<공간>.<블롭>`: 그 블롭을 알린 메시지)과 메시지마다 알린
  블롭(`archive.evref.<공간>.<메시지>`). 받아 둔 공간은 `node.spaces`에 넣지 않습니다(상태를 풀 수 없음). 명단의 멤버
  기기와 인증되면 그 기기에게서 공간·보관 채널 스트림을 당겨 오고, 첨부는 일반 블롭 스웜으로 받아 고정합니다. 용량 때문에 본문을
  빼고 받은 메시지는 저장소의 시더 제공 제외 표시(`seed_excluded`)로 기록해 내주지 않습니다. 주기 작업은 채팅 수명이 지난 본문
  지우기(작성 시각 기준), 72시간 기다린 빠진 채널 지우기(`Store::purge_stream`), 사용량 다시 세기(`Store::space_event_bytes` + 첨부
  크기, 지운 뒤와 10분마다)·저장, 때가 된 첨부 다시 시도(동시 16개, 물러나며 8번)입니다. 해제·`archive.drop`과 엇갈린 받기가 남긴
  것은 받은 뒤 받아 둔 공간인지 다시 보고 지웁니다.
  바탕 체크포인트의 스냅숏 블롭 알림은 `archive.snap.<공간>`에 최근 2개만 두고(밀려난 블롭은 다른 쓰임이 없으면 지움), 명단의
  멤버 기기 누구에게나 내줍니다. 명단 차이는 받아 둔 공간 기록(`HostedRoster.base`·`deltas`)에 모읍니다.
- 저장은 모두 일반 등급 저장소(`PersistentEvent`)라 휘발성 데이터는 타입으로 막힙니다. 로그에는 공간·기기 ID와 오류만 남깁니다.

## 시험

`tests/core.rs`(코어 흐름), `tests/admin.rs`(관리·기기), `tests/crews.rs`(크루), `tests/blobs.rs`(블롭·정책), `tests/volatile.rs`
(휘발성 채널·진단, 노드 3개), `tests/volatile_blobs.rs`(휘발성 첨부: 주고받기·저장 확인, 삭제·퇴장·만료 때 RAM 비움, 만료 전후로
디스크·임시 파일에 표식 없음, 늦은 입장자, 보낸 기기가 꺼진 뒤, 크기 한도·RAM 예산, 차단, 묶지 못한 첨부의 메시지는 나가지
않음, 받다 멈춘 첨부의 예산 줄이기·포기, 일반 첨부와 나눠 쓰는 내려받기 자리, 노드 3개), `tests/net_features.rs`(통화·Tor·위치 기록), `tests/moderation.rs`(밴·차단·신고, 노드 3~4개), `tests/storage.rs`(저장 공간 정리 순서·고정·다시 받기, 미리 받기와 한도, 동영상·음성 힌트 검사, 확정 기기 후보), `tests/tor_leak.rs`(Tor 스웜에서 미끼 수신기에 아무것도 닿지 않음과 양성
대조), `tests/perf.rs`(성능 목표·시뮬레이션은 `#[ignore]`, gossip 중계 회귀 시험만 기본), `tests/integration.rs`(영역을 넘는 시나리오: 위치 기록으로
찾은 새 멤버와 비공개 첨부, 자동화 토큰의 추방 확인 → 통화·휘발성에서 빠짐, 연결한 기기의 크루 합류, 크루 아이콘·이모지 블롭,
실행 중 Tor 전환의 블롭·정책 반영, 폐기한 기기가 통화·휘발성·그룹에서 빠짐, 휘발성 첨부는 저장 공간 정리·고정과
무관함(정리해도 RAM 그대로, 고정은 `1004`), 보관 노드는 휘발성 메시지·첨부를 받지 않음(블롭 요청 거절, 명단에서 빠짐, 디스크에
표식 없음), 보관 노드가 받아 둔 첨부는 정리되지 않고 보낸 기기가 꺼진 뒤 멤버를 다시 채움), `tests/checkpoint.rs`(체크포인트: 세 멤버가 같은 바탕·상태·스냅숏 블롭, 이어지는 체크포인트, 새 멤버가 고정에서 사슬을 확인하고
스냅숏과 경계 위만 받아 같은 상태로 들어와 다른 멤버와 견주고 글을 씀, 재시작이 스냅숏에서 같은 상태로, 위조한 `GetDeviceKeys`
답·묻지 않은 기기 버림. 문턱을 낮춘 설정과 `testkit::checkpoint::hold`), `tests/archive.rs`(X3: 보관 노드만으로
따라잡기, 새 멤버의 공개 기록 읽기, 바깥 노드 거절·해제 뒤 멈춤, 다시 시작한 소유자, 권한 없이 닿는 밴, 본문을 뺀 이벤트와
사용량 다시 세기), `tests/sfu.rs`(X2: 호스트 선출·층 넘기기·넘겨주기, 시더 호스트·참가자 아닌 노드 거절, Tor로 도는 기기에서
맡지 않음, 참가하지 않은 호스트의 권한·수용 인원 나눠 쓰기), `tests/robustness.rs`(W3: 폐기된 기기의 기기
추가 거절과 스스로 지우기, 시드만 가진 기기를 친구·멤버·내 기기가 받지 않음(`seed_alone_is_refused_by_friends_members_and_own_devices`), 연결한 기기의 폐기와 스스로 지우기, 내 기기가 알려 준 1:1 공간과 친구 기록, 확정 기기 표 넘기기와 잃은 뒤 전체 재키로 되살리기, 동시 가입을
갱신 하나로, 실패한 휘발성 키 갱신 재전송, 4MiB를 넘는 메모, 폐기된 기기의 위조 판 바로잡기·상자 키 바꿔치기, 연결한
기기가 폐기된 기기의 판을 무시함, 오프라인 사이 더해진 기기의 폐기로 지우기, 관리자에게 넘긴 확정 기기와 심어 둔 키 갱신,
시작할 때 옛 OTP 비밀 지우기(`old_otp_secrets_are_wiped_on_start`))와 `tests/common`(도구). 기기 연결은 `tests/net_features.rs`가 봅니다(`new_device_joins_by_the_link_alone`,
`refused_device_gets_no_identity`, `second_device_on_one_link_is_flagged_and_gets_nothing`, `ended_link_is_refused`, `join_reports_an_unreachable_device`,
`first_sync_ends_with_profile_friends_and_crews_in_place`, `own_devices_link_directly_until_revoked`). 시험의 새 기기는 `common::linked`·`common::link_device_with`로
들이고 `common::wait_own_device`(내 다른 기기의 `device.list`)로 기다립니다. 폐기될 기기의 키는 `testkit::capture_keys`로 미리
잡아 둡니다(`CapturedKeys::sign`, `forge_device_list`로 위조 판을 만듦). 시드를 훔친 기기는 `testkit::install_stolen_seed`입니다. 형식 호환은 `yangpa-core`의
`lists_written_before_otp_still_decode_and_verify`·`otp_key_gates_recovery_and_round_trips`가 봅니다.
여러 사람 크루·설정을 고른 채널·휘발성 RAM 들여다보기(세대·암호문만)는 `testkit::volatile`, 통화·Tor·위치 기록은
`testkit::net`. 한 프로세스에 노드 여러 개를 띄워 로컬 API(`Node::call`)로 부르고, `eventually`/`next_notification`으로
조건을 기다립니다. 다시 띄울 때는 같은 포트를 써서 상대의 주소 기록이 맞게 합니다.

## 알려진 빈칸

영역 B1~B5, W1~W5, X1~X5를 모두 합친 뒤의 상태입니다. 기획서의 미결정 사항은 `plan.md` 끝, 기획서와 달라진 점은 `decisions.md`.
두 번째 묶음(W)에서 메운 것:
- W1: 데스크톱 앱이 실제 데몬 계약을 따름(`methods.generated.json` 드리프트 검사), 모더레이션·차단 화면, 사이드카 묶음,
  실제 `yangpad` 셋을 띄우는 종단 간 시험(`apps/desktop/src/e2e`).
- W2: 밴·차단·신고(`moderation`, `tests/moderation.rs`), CLI·MCP 도구.
- W3: 기기 목록 공동 서명·폐기·폐기된 기기 스스로 지우기, 확정 기기 인계·그룹 다시 세우기, 동시 가입을 커밋 하나로, 휘발성 키 갱신 재전송, 4MiB를 넘는 메모 나눠 보내기(`tests/robustness.rs`).
- W4: Tor 누출 시험(`tests/tor_leak.rs`, `yangpa-net/tests/tor_leak.rs`), Arti 경유 링크 미리보기, 성능 목표 시험·시뮬레이션
  (`tests/perf.rs`), 명령 수 회귀 검사(`yangpa-bench`), 만나지 않은 멤버의 gossip 중계.
- W5: 릴리스 파이프라인, 헤드리스 시더(Docker·systemd), 사용자 문서, Windows pipe 서버 사용자 확인.

세 번째 묶음(X)에서 메운 것:
- X1: 휘발성 채널 첨부(`volatile/attach.rs`, RAM에만 올려 참가자끼리 받고 내주며 메시지와 함께 지움, 저장은 경고 확인 뒤,
  `tests/volatile_blobs.rs`), 데스크톱 첨부 단추(메모리에만 보관).
- X2: 통화 SFU(`crates/yangpa-sfu`, `sfu.rs`, `voice.rs`의 메시/SFU 결정·호스트 선출·히스테리시스·넘겨주기, Tor로 도는 기기에서
  맡지 않음, `tests/sfu.rs`), 헤드리스 시더의 호스트 먼저 맡기, 데스크톱 SFU 모드(화면 simulcast·받을 층 고르기).
- X3: 보관 전용 노드(`archive`, `tests/archive.rs`), CLI `yangpa archive …`, MCP `list_archives`·`designate_archive`·
  `revoke_archive`, 데스크톱 크루 설정 → 시더 → 보관 전용 노드.
- X4: 받은 첨부 저장 공간 정리·고정, 헤드리스 미리 받기, 앱이 만든 동영상·음성 썸네일·길이, 데스크톱 확정 기기 화면(`tests/storage.rs`).
- X5: 설치 파일이 옛 데몬 끄기(MSI WiX 조각, deb 관리 스크립트, 설치 시험 `scripts/installer-smoke.*`), 릴리스 대상 넓히기(리눅스
  ARM64·macOS 유니버설·arm64 Docker 이미지), 실제 Tor 망 검사를 크루·첨부·미리보기·통화로 넓힘, 실제 창 시험(`e2e-window`).
- 합친 뒤 영역 경계 시험(`tests/integration.rs` 7~9번): 휘발성 첨부와 저장 공간 정리·고정, 보관 노드와 휘발성 데이터, 보관
  노드의 고정 첨부와 정리·다시 받기. SFU와 Tor의 경계는 `tests/sfu.rs`의 `tor_mode_and_hidden_ip_refuse_hosting`.

아래는 남은 것.

- 보관 전용 노드(X3):
  - 비공개 그룹 갱신(키 그룹 스트림)과 놓친 키 중계는 보관 노드가 하지 않아, 그런 키에는 멤버 기기가 한 번은 온라인이어야 합니다.
  - 지운 메시지의 수정(`Edit`) 이벤트 본문은 보관 노드에 남습니다(삭제 반영은 대상 메시지만).
  - 보관 노드는 첨부 받기를 8번까지 물러나며 시도하고, 그 뒤에는 새 메시지가 그 첨부를 알릴 때까지 기다립니다.
  - 첫 명단(지정)의 소유자는 지정한 관리자가 적은 대로 고정됩니다(보관 노드는 상태를 읽지 못함). 멤버가 보관 노드의 명단을 읽어
    볼 방법(명단 조회 요청)은 없습니다.
  - 나간 멤버의 기기가 쓴 이벤트는 작성자를 확인하지 못해 용량 상한으로만 묶입니다.
  - 보관 노드가 본문을 빼고 받은 이벤트는 내주지 않아, 멤버는 다른 시더에게서 받을 때까지 그 ID를 다시 묻습니다.
  - 보관 노드 기기로 그 크루에 가입하는 것은 지원하지 않습니다(가입하면 지정이 지워지고 받아 둔 공간과 섞임).
  - 보관 노드 접속 정보에는 직접 IP가 없어, 멤버는 피어 캐시·위치 기록·DHT로 찾거나 보관 노드가 먼저 연결해야 합니다.
  - 퇴장 시 삭제(`OnLeave`) 채널의 기록은 보관 노드에서 지우지 않습니다(읽는 기기의 규칙).

- 기기(B4, W3):
  - 기기를 모두 잃으면 계정은 사라집니다. 사용자는 외부 연락으로 친구·크루에게 친구 삭제·차단·강퇴를 부탁합니다.
  - 신원 키는 모든 기기에 있지만, 기기 목록 판은 그 판을 만든 기기의 공동 서명이 있어야 받습니다. 폐기된 기기가
    만든 판(기기 추가·자기 되살리기)은 폐기를 아는 노드가 거절하고, 내 활성 기기는 그 기기가 더한 기기를 폐기한 판으로
    바로잡습니다. 폐기를 아직 모르는 노드(폐기 전 목록만 아는 친구)는 그 판을 받을 수 있고, 바로잡은 판이 닿을 때까지 그
    기기들을 활성으로 봅니다.
  - 시드를 가진 사람(폐기된 기기, 훔친 기기)이 새 기기 키를 만들어 자기를 더한 판은 이 빌드가 받지 않습니다(친구·멤버·내 기기 모두).
    다만 크루 해소기는 옛 기록을 다시 적용할 때 결과가 같아야 해서, 대상 말고 멤버 두 명이 따로 낸 복구 판을 그대로 받습니다
    (옛 기록 재적용용으로 남은 규칙). 그래서 옛 빌드 멤버나 공모 멤버를 거치면 목록에 OTP 키가 없는 계정의 그 기기가 크루 상태에 들어갈 수
    있습니다. 이 빌드의 노드는 그 기기의 인증을 받지 않고(`Unknown`), 그 사용자의 활성 기기가 폐기한 판을 내면 빠집니다(decisions.md "기기 연결").
  - 옛 빌드의 친구는 업데이트할 때까지 복구 판을 자동으로 받습니다. 옛 빌드 친구가 가진 OTP 맡김은 이쪽에서 지우지 못하지만, 이 빌드는 코드를 보지 않아 쓸 곳이 없습니다.
  - 승인한 뒤 새 기기가 묶음을 받아 가지 못해도 그 기기는 내 목록에 활성으로 남습니다(`device.revoke`로 폐기).
  - 다른 사용자가 알려 준 폐기 판으로 지우는 경로(`vouched_revocation`)는, 시드를 가진 사람과 짠 다른 사용자가 이 기기를
    폐기한 판(새 기기 키로 공동 서명)을 `Revoked`로 보내면 이 기기를 지우게 할 수 있습니다(데이터를 지울 뿐 읽지는 못함).
  - 폐기된 기기는 다음에 연결해 폐기를 알게 되면 로컬 데이터를 지웁니다. 끝내 연결하지 않는 기기의 데이터는 지울 수 없고(분실·
    도난 기기는 원래 그럴 수 있음), 폐기 전에 받은 기록은 그 기기에서 읽힙니다.
  - 키 그룹: 보내는 쪽 규칙은 `rotation_wait`(30초) 뒤 옛 키로 보냅니다(`strict_rotation`을 켜지 않은 크루, protocol.md "기록 키"의
    남은 위험). 진 갈래의 상태는 순번마다 최근 64개만 남겨, 그보다 오래된 갈래 변경 뒤에는 그 갈래 키로 쓴 메시지를 중계로만
    읽습니다(체크포인트 전의 상한 있는 대체). 예비 기기(`KeyBackupSet`)를 정하는 로컬 API가 아직 없습니다.
  - 친구를 끊어도 지워지지 않는 것: 프로필 이미지 캐시 블롭, 그 사람의 주소·마지막 접속 기록, 묘비(공간 ID만, 1024개까지 내 기기에 전함).
- 피어 찾기·인증: 모르는 기기의 주소는 인증한 적이 있거나 위치 기록(DHT), 없으면 인증된 멤버의 소개(`FindDevice`)로 찾음
  (`locator::lookup`). 모르는 피어에게는 내 신원을 밝히지 않으려고 연결만 된 피어를 먼저 인증하지 않음(가입 전에 맺은 연결
  포함). 앱 기기(Tor)는 DHT 서버가 없어 멤버 소개로만 서로를 찾음. 그래서 소개해 줄 멤버와도 연결되지 않은 노드는 새 크루
  멤버와 직접 전하지 못하고 주기 따라잡기로만 닿음. 시험은 `testkit::authenticate`로 직접 인증하거나(`tests/crews.rs`) DHT 노드를
  두거나(`tests/integration.rs` 1번), DHT 없이 멤버 소개로 찾음(`tests/net_features.rs`의 `crew_members_meet_through_a_member_without_dht`).
- 볼 수 없는 채널의 메시지도 기록 키를 받으면 보기 행은 만들어짐(API·알림은 `VIEW_CHANNEL`로 거름). 볼 수 없는 멤버가 있는
  채널은 채널 그룹으로만 기록 키를 나누므로 그 멤버는 새 키를 받지 못함. 모두가 보던 채널이 나중에 일부에게 막히면, 그 전에
  공간 그룹으로 나눈 키는 이미 받은 그대로 남음.
- 크루(B1):
  - 새 확정 기기가 공간 그룹에 없던 기기면, 그 그룹에 든 기기 하나가 온라인이 되어 상태 키를 넘겨줘야 상태를 열고 그룹을 다시
    세움(그 전까지 멤버 변경은 대기). 채널 그룹은 키 없이도 다시 세우지만, 지난 기록 키를 넘겨받기 전까지 새 확정 기기와 새로
    넣은 기기는 그 채널의 지난 기록을 못 읽음. 소유권 이전은 넘기는 쪽이 미리 넣으므로 이 길을 타지 않음.
  - 받는 쪽은 메시지·키 묶음 작성자의 권한을 받을 때의 상태로 봄. 그래서 채널을 볼 수 없게 된 사람(예: 넘긴 뒤의 옛 소유자)이
    전에 쓴 메시지는 나중에 처음 받는 노드가 거절함.
  - 1:1→크루 전환 뒤 친구 기록은 대화를 열 때까지(`friend.open`) 그 크루를 가리킴. 열면 둘만의 1:1 공간을 새로 만들어 기록을
    옮기고 `Social(DmSpace)`로 상대에게 보냄(protocol.md "크루로 바꾼 뒤 다시 1:1"). 옮기기 전 `friend.remove`는 크루에서
    나가지 않고 친구 기록만 지움(공간 종류로 가림). 그 크루에서 나가면(`crew.leave`) 공간을 지울 때 저장소가 기록의 공간을
    비우므로 나간 뒤 바로 새 1:1 공간을 만듦. 내 다른 기기는 기기 사이 공간 맞추기로 따라옴(protocol.md "내 다른 기기로 전하기"). 기록이
    크루를 가리키는 동안은 내 다른 기기에 새로 만들지 않음(그 기록의 삭제는 다른 기기에 전해지지 않아 되살아나지 않게).
  - 가입 대기 중(상태를 풀기 전)인 공간은 `space.list`에 없고 `space.get`은 `1004`.
  - 잠긴 게시글의 댓글 금지는 API에서만(적용 경로는 게시글 상태를 모름). 비공개 포럼의 고정·잠금 변경은 상태 레인이라 게시글
    ID가 채널을 못 보는 멤버에게도 보임.
  - 남의 게시글 제목·태그 변경의 `MANAGE_MESSAGES`는 작성 시점이 아니라 적용 시점 상태로 판단.
- 블롭·정책(B3):
  - 데몬은 영상 디코더가 없어 동영상 썸네일과 WebM·MKV·오디오 형식의 길이를 앱이 넘긴 값으로만 채움(MP4·MOV 길이는 데몬이
    읽음). 데스크톱 앱은 웹뷰가 재생할 수 있는 형식만 읽으므로 그 밖의 형식이나 CLI·MCP로 보낸 동영상은 썸네일 없이 감.
  - 저장 공간 정리는 블롭 단위라 반쯤 받은 블롭도 통째로 비우고, 다시 받으면 처음부터 받음. 정리 대상 크기는 디스크에 있는
    암호문 조각 기준이고 DB·매니페스트·썸네일은 세지 않음. 키 확인을 아직 못 한 쓰임만 있는 블롭을 비우면 `BlobHave`가 빈
    비트맵 대신 `NotFound`로 답함(둘 다 "없음"이라 시더 수는 맞음). 공간 이모지·아이콘을 바꾸거나 지워도 예전 블롭은 남음.
  - 미리 받기 보충은 한 번이라도 받으려 한 첨부(정리로 비웠거나 자리가 없어 그만둔 것, 저장소 행이 남음)를 다시 넣지 않으므로,
    한도를 늘린 뒤에도 그 첨부는 `blob.fetch`(앱에서는 첨부의 받기)로 받아야 함.
  - Tor로 도는 기기의 링크 미리보기는 Arti로만, 링크마다 5초 안에 조회하므로 회로를 새로 만드는 동안 시간을 넘기면 미리보기 없이 보냄.
- 휘발성(B2): 갱신 키 직접 전송은 6번(최대 5초 간격)까지 다시 보내고 그 뒤에는 연결 끊김을 기다림(그동안 그 기기는 새 세대를
  못 읽음). 참가자끼리 일부만 연결된 경우 기기마다 참가자·갱신 담당이 달라 볼 수 있는 메시지가 갈릴 수 있음. 스냅숏은 511개·6MiB까지.
- 휘발성 첨부(X1):
  - RAM 예산은 노드 전체 하나라(채널·공간별 몫 없음) 큰 첨부가 많은 채널 하나가 다른 채널의 첨부 받기를 막을 수 있음(만료되면 돌아옴).
  - 첨부 암호문은 보통 프로세스 메모리라(세션 키만 잠긴 메모리) 운영체제가 스왑·최대 절전 파일에 쓸 수 있음. 썸네일을 만들 때의
    디코더 버퍼와 웹뷰가 그린 이미지도 덮어쓰지 못함.
  - 보낸 기기가 나가고 가진 참가자도 없으면 받을 수 없음(10분 뒤 멈추고 받다 만 조각도 지움, 메시지는 남음).
  - 휘발성 첨부가 일반 내려받기 자리를 먼저 가져가므로, Tor로 도는 기기(자리 1개)에서 휘발성 첨부가 계속 들어오면 일반 첨부 받기가 그동안
    밀림.
  - 대기열이 가득 차 자동 받기에서 빠진 첨부와 예산이 모자라 자동 받기를 그만둔 첨부는 알림 없이(디버그 로그만) 받지 않은 채로
    남음(`blob.status`로 보이고 `blob.fetch`로 다시).
  - 참가자인 내부자가 첨부의 블롭 ID와 키를 일반 채널 메시지에 베껴 적으면, 그 첨부를 RAM에 가진 다른 참가자의 일반 받기 경로가
    암호문을 디스크에 받을 수 있음(평문을 일반 채널에 다시 올리는 것과 같은 내부자 행위. `diag.leakcheck`의 `blob-store:`가 알림).
  - 디스크 블롭을 가리키는 휘발성 메시지(블롭 ID를 베낌)는 `key_from`이 맞을 때만 휘발성으로 봐 일반 첨부를 가리지 않지만,
    `blob.status`·`fetch`는 이 공간에서 디스크 쪽 쓰임이 없는 블롭만 휘발성으로 봄.
- 통화·망(B5):
  - 실제 Tor 망을 쓰는 시험은 `#[ignore]`(`net_features::real_tor_friends_over_onion`, `tor_leak::real_tor_link_preview`,
    `yangpa-net`의 `tor_mode_restart_in_process_relaunches_onion`: 같은 프로세스에서 다시 띄운 onion 서비스에 실제로 접속).
    "Tor 스웜에서 일반 IP로 나가는 연결 0건"은 CI에서 127.0.0.1 미끼 수신기로(`tests/tor_leak.rs`, `yangpa-net/tests/tor_leak.rs`,
    양성 대조 포함), 실제 망에서는 주간 `tor-leak.yml`이 네트워크 이름공간에서 패킷을 캡처해(`scripts/tor-leak-check.sh`) 봅니다.
    실제 망 검사는 데몬 둘의 친구 맺기·DM·크루 만들기·초대·참여·크루 메시지(첨부·링크 미리보기)·통화(P2P 전용이 아닌
    채널의 Tor 통화, 소리 패킷 포함)를 단계별로 캡처해 봅니다(UDP·DNS·IPv6 0건, 같은 시나리오를 `--no-tor`(데몬은 `--direct`, 통화는
    STUN 목록을 받음)로 루프백에서 먼저 돌림). 맨 뒤에 새 기기를 이어 내 기기 직접 연결을 돌리고, 그 단계부터 STUN 목록의 서버·UPnP 찾기·내
    공인 IP로 가는 UDP만 따로 세어 넘깁니다. 남은 것: 참여한 P2P 전용 채널의 통화·P2P 스웜은 IP를 드러내는 것이 정해진 동작이라 보지 않음.
    브리지는 돌리지 않음(Snowflake는 목적지로 판정할 수 없음). 전송 TLS의 키 교환 방식은 캡처로 확인하지 않음(Tor 회로 안).
    미리보기가 Tor 회로 시간(5초) 안에 못 만들어지면 실패가 아니라 경고만 남김. 실제 Tor 망 실행은 GitHub 러너에서만 돕니다.
  - gossip 허용 목록은 인증해 본 기기의 노드뿐이라, 큰 크루에서 메시가 허용 목록 안의 몇 노드(가입을 받아 준 노드 등)에 몰림.
    만나지 않은 멤버의 메시지는 그 노드들이 전달해 주어야 실시간으로 닿음. gossip이 놓친 이벤트는 주기 따라잡기(기본 5분)로만
    채워지고, 피어가 적은 노드·막 들어온 멤버는 메시에 늦게 들어 놓치기 쉬움(`tests/perf.rs` 시뮬레이션, docs/performance.md).
- SFU 통화(X2):
  - 호스트 모드는 설정·명령줄(`--sfu-host`)로만 정합니다(실행 중에 바꾸는 API 없음). 전환 기준(`mesh_max`·`mesh_min`)은
    `NodeConfig`에만 있고 명령줄 인자는 없습니다.
  - 소켓과 후보(호스트·서버 반사)는 실행기를 시작할 때 한 번만 정합니다. 노드 IP나 STUN 서버 목록이 바뀌어도 실행기가 멈췄다
    다시 뜰 때(데몬 재시작 등)까지 예전 주소를 알립니다. TURN은 쓰지 않아, 호스트도 참가자도 직접 닿지 않는 NAT 조합은
    통화가 이어지지 않습니다.
  - 후보 조건(`preferred`·`public`·`rtt_ms` 등)은 각 노드가 스스로 알리는 값이라 참가자(또는 `manage_voice`를 받은 노드)가
    좋게 꾸며 호스트가 될 수 있습니다. 호스트는 모든 참가자의 IP·말하는 때·트랙 구성을 보고, 참가자 호스트는 프레임 키도
    있어 다른 참가자 이름으로 프레임을 만들 수 있습니다(protocol.md "SFU"). 수용 인원이 참가자 수보다 큰 후보가 없으면
    메시로 남습니다.
  - 결정(`SfuPlan`)을 받는 쪽은 호스트의 `manage_voice`를 보지 않습니다(권한 변경과 엇갈려 닿을 수 있음). 권한 없는 노드를
    호스트로 고르는 관리 기기는 이미 참가자라 같은 메타데이터를 압니다.
  - 시더가 이미 있던 통화에 늦게 나서는 것(권한을 나중에 받음, 소켓을 다시 열 수 있게 됨)은 20초마다 다시 보기에 기댑니다
    (공간 상태가 바뀔 때 바로 다시 재지는 않음).
  - 참가하지 않고 보기만 하는 노드의 `voice.participants`는 늘 `topology: "mesh"`입니다(결정은 참가자·호스트 후보에게만 감).
  - 실제 Tor 망에서 맡지 않는지는 시험하지 않습니다(`tests/sfu.rs`는 Tor 망 없이 `enforce_policy`로 같은 정책을 탐).
  - 앱은 아직 카메라를 보내지 않습니다(엔진·신호는 `camera` 출처를 지원). 화면 simulcast 층을 실제로 몇 개 보내는지는 웹뷰가
    정합니다(작은 화면이면 낮은 층만).
- 모더레이션(W2):
  - 차단 목록은 기기마다 따로입니다(내 다른 기기와 맞추지 않음). 차단해도 상대는 같은 크루 멤버로서 다른 시더에게서 내 메시지를
    받을 수 있고, 내 노드도 상태·키·키 합의 레인은 줍니다.
  - 차단한 사람의 새 메시지는 `message.new`를 보내지 않으므로 화면은 채널을 다시 읽을 때 접힌 메시지를 봅니다.
  - 신고하며 숨긴 메시지를 다시 보이게 하는 API가 없습니다(공간마다 1만 개까지).
  - `delete_recent`는 작성자가 적은 작성 시각으로 고르므로, 시각을 과거로 꾸며 쓴 메시지는 빠질 수 있습니다(그런 메시지는
    `message.delete`로 지움). 이미 다른 기기·사람에게 넘어간 사본은 되돌릴 수 없습니다. 밴 시각 자체도 작성자가 적는 값이라
    `ban` 권한이 있는 사람이 꾸미면 과거의 다른 7일 구간을 고를 수 있습니다(밴 하나에 7일, 상태에 기록이 남음).
  - 밴을 적용한 뒤에 도착한 그 구간의 메시지(밴이 풀리고 다시 들어온 사람의 예전 메시지가 늦게 온 경우)는 지우지 않습니다.
  - 차단한 사람이 같은 친구 링크로 다시 보낸 것을 알아보는 기록은 RAM에만 있습니다. 내 노드를 다시 시작한 뒤 같은 1회용 링크로
    다시 보내면 보통 흐름('대기 중')과 달리 '쓸 수 없는 링크'를 받습니다.
  - 밴 목록(사유 포함)은 상태에 있어 멤버 누구나 자기 노드에서 읽을 수 있습니다(API만 `ban` 권한으로 가림).
  - 신고는 보낼 때의 운영진에게만 갑니다. 나중에 운영진이 된 멤버는 지난 신고를 받지 않고, 처리 결과만 운영진끼리 나눕니다.
    운영진이 아닌 신고한 사람은 처리 결과를 받지 않습니다.
  - 운영진 기기에 그 메시지 사본이 없으면(못 받은 기록 구간 등) `dismiss`만 할 수 있습니다.
- 앱·도구(W1, W2, W3):
  - MCP 서버에는 기기 폐기·확정 기기 도구가 없습니다(`device.revoke`는 사람 전용이라 일부러 뺌, 확정 기기는
    데스크톱 크루 설정·CLI `crew committer`로).
  - 차단한 사람의 새 메시지는 알림이 없어 앱은 채널을 다시 읽을 때 접힌 줄을 봅니다(위 모더레이션).
  - 데몬 없이 브라우저에서 띄우는 모의 모드는 화면 설정을 `localStorage`에 둡니다(Tauri 앱은 쓰지 않음, 휘발성 ID는 어디에도
    저장하지 않음).
- 배포(W5):
  - 릴리스 파이프라인·Docker 이미지는 CI에서만 돌고 아직 실제 태그로 돌려 보지 않았습니다. 묶음은 코드 서명이 없습니다(macOS는
    임시 서명). 리눅스 ARM64·macOS 유니버설·arm64 이미지는 만들지만, MSI·NSIS 설치 시험과 dmg 확인은 GitHub 러너에서만 돌고
    deb·AppImage 시험만 로컬에서 돌려 봤습니다.
  - Windows 설치 파일(MSI·NSIS)은 설치하는 사용자 계정이 띄운 데몬만 끕니다. 다른 계정의 데몬은 재부팅 뒤 교체로 갑니다.
    deb은 파일이 바뀐 뒤 옛 데몬을 끄지만 열려 있던 앱이 새 데몬을 다시 띄우지 않아 앱을 다시 시작해야 합니다. AppImage는
    끄지 않습니다.
  - 실제 창 자동 시험은 리눅스 WebKitGTK(Xvfb)에서 신원 만들기 → 메인 화면까지입니다. macOS·Windows 창, 통화·화면공유 장치,
    서명·공증 경고는 사람이 확인합니다(CONTRIBUTING.md "릴리스 절차").
- 성능(W4):
  - 명령 수 기준선(`crates/yangpa-bench/baseline.json`)은 W2·W3를 합치기 전에 쟀습니다. 적용 경로(`ingest_batch_20`)가 바뀌었으니
    CI `bench.yml`의 `head.json`을 받아 기준선으로 옮겨야 합니다(PR은 기준 커밋을 다시 재므로 영향 없음, 예약 실행만 엄격 비교).
  - Tor 경유 전달 지연·스크롤·음성은 재지 않았고, 지연·손실을 건 시뮬레이션(netem)은 CI에서만 돕니다(docs/performance.md).
