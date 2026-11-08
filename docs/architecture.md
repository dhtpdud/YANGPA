# 아키텍처

이 문서는 [기획서](plan.md)를 코드 구조로 옮긴 설계입니다. 크레이트 경계와 공개 API가 이 문서의 계약이며,
프로토콜·암호 세부는 [protocol.md](protocol.md), 로컬 API는 [api.md](api.md), 기획서와 달라진 점은
[decisions.md](decisions.md)에 있습니다.

## 전체 구조

```
apps/desktop (Tauri + React/TS) ─┐
yangpa (CLI)                     ├── JSON-RPC 2.0 (Unix socket / named pipe) ──> yangpad (코어 데몬)
yangpa-mcp (AI 연동, MCP stdio) ─┘                                                   │
                                                                                      ├─ yangpa-store  (SQLCipher·블롭·휘발성 RAM)
                                                                                      ├─ yangpa-crypto (AEAD·봉인 상자·키 그룹 키 일정·잠긴 메모리)
                                                                                      ├─ yangpa-zl     (기기 사이 데이터 압축: OpenZL v0.3.0)
                                                                                      ├─ yangpa-sync   (DAG·지문 비교·상태 해소·시더/피어 정책·스웜 스케줄러)
                                                                                      ├─ yangpa-sfu    (통화 SFU 엔진: str0m, sans-IO, 프레임을 풀지 않고 넘김)
                                                                                      └─ yangpa-net    (libp2p: QUIC/TCP·Kademlia·GossipSub·Relay·DCUtR·AutoNAT·mDNS·Tor)
음성·화면공유 미디어: 웹뷰 WebRTC (협상은 데몬의 libp2p 스트림 경유, 프레임 E2EE 키는 키 관리 기기의 데몬이 만들어 참가자에게 봉인해 배포, decisions.md)
  소규모는 메시(참가자끼리 직접), 큰 통화는 한 노드의 데몬 안 SFU(yangpa-sfu)에 참가자마다 연결 하나(protocol.md "SFU")
```

의존 방향: `core ← crypto ← store`, `core ← sync`, `core ← net`, `zl ← crypto`, `zl ← net`, `core ← rpc`, `core ← sfu`, 그리고 `daemon`이 전부를 묶습니다.
`cli`·`mcp`·데스크톱 앱은 `rpc`만 씁니다(데몬 내부에 직접 접근하지 않음). `bench`(성능 기준)는 시험 도구라 데몬과 하위
크레이트를 직접 씁니다.

## 크레이트별 책임과 공개 API

### yangpa-core (작성 완료)

공용 계약. 식별자(`ids`), 서명(`sign`), 이벤트(`event`), 본문(`body`), 모델·권한(`model`), 기기 목록·프로필(`device`),
링크(`links`), 이모지 표기(`emoji`), 블롭 매니페스트(`blob`), 노드 간 프로토콜(`wire`).

- 서명은 Ed25519 + FN-DSA-512 혼합(`sign::SigningKeypair`, `sign::Sig` 1659바이트, `sign::verify(키 ID, …)`). 사용자·기기
  ID는 두 공개키를 묶은 키 ID(`sign::key_id`)이고, 서명에 공개키가 실려 ID만으로 검증합니다(protocol.md "혼합 서명").
  채널 스트림 이벤트는 공개키를 뺀 짧은 서명(730바이트, `sign_slim`·`Sig::is_slim`)이고 받는 쪽이 상태 기록의 키로
  확인합니다(`SignedEvent::verify_with`, `sign::verify_with_keys`). `SignedEvent::verify`는 긴 서명만, `check_integrity`는
  서명 없이 모양·본문 해시·서명 길이가 스트림에 맞는지만 봅니다.
- 이벤트 ID = BLAKE3(헤더 || BLAKE3(서명의 FN-DSA 부분)). 본문은 `body_hash`로만 묶여, 본문을 버려도 DAG가 유지됩니다.
- 부모 해시는 같은 **스트림**(공간 수준 또는 채널)의 이벤트만 가리킵니다. 채널 단위로 시더가 제공을 빼도 다른
  채널의 DAG가 끊기지 않게 하기 위해서입니다. 논리 시계는 공간 전체 기준입니다.
- 이벤트 헤더는 판 하나(`EVENT_VERSION` = 3, 필드 14개, 파생 serde라 postcard가 필드 순서대로)입니다. 다른 판은
  `check_shape`가 `UnsupportedVersion`으로 거릅니다(예전 판 이벤트는 풀지 않음, decisions.md "예전 이벤트"·"키 갱신 1부").
  `KeyRef::{State(StateKeyId), Record{root: ChannelRootId}, Volatile{generation}, Sealed}`(평문 본문 없음). `check_shape`는
  `KeyUpdate`가 `State`이고 채널이 없을 것, 키 그룹 범위면 키 레인은 `State`·`Sealed`만을 봅니다.
- 부분 전달 스트림 `StreamId::Inbox(UserId)`·`StreamId::KeyGroup(KeyGroupId)`(`StreamId::is_scoped`): 헤더 끝의
  `scope: Option<Scope>`와 `state_anchor: Vec<EventId>`(작성자가 마지막으로 적용한 상태 이벤트 1~`MAX_STATE_ANCHOR`개)로
  정해지고, `EventHeader::stream()`은 범위가 있으면 그 스트림입니다. 키 그룹 ID는 `ids::KeyGroupId` 하나(키 그룹 형식
  `keyupdate`도 같은 타입): 크루 그룹 `KeyGroupId::crew(space)`(= 공간 ID, 공간 스트림으로 가므로 `Scope::KeyGroup`에 실을 수
  없음), 비공개 그룹 `KeyGroupId::private(space, ChannelSettings::view_rule())`(라벨 `yangpa/v3/key-group`).
  `SpaceState::channel_key_group(space, ch)`(비공개 그룹, 보기 덮어쓰기가 없고 @everyone이 보는 채널은 크루 그룹을 써서 `None`),
  `SpaceState::has_key_group`·`can_view_key_group`(protocol.md "부분 전달 스트림"). 데몬의 채널 키 고르기·확정 기기의 원하는
  그룹·따라잡기·시더·해소기의 그룹 자격이 모두 이 함수 하나를 씁니다.
- 레인: Message(시더·피어 설정 적용), Key(채널 열람자에게 필수), State·KeyAgreement(모든 멤버 필수). 키 합의 레인
  (`Lane::KeyAgreement`, 코드 80~99)에는 `KeyUpdate`(82, 본문 한도 `MAX_KEY_UPDATE_BODY_LEN` 960KiB)가, 상태 레인에는
  `KeyBackupSet`(75)과 체크포인트 이벤트(74·76·77)가 있습니다. 옛 MLS 코드 80·81은 다시 쓰지 않습니다(`EventKind::Other`).
- 기기 목록은 늘기만 합니다: `DeviceList::extends(old)`(새 판이 예전 항목을 모두 갖고 폐기를 되돌리지 않음),
  `DeviceList::merge(other)`(서로 모르는 내 두 판을 결정적으로 합침), `DeviceList::is_revoked(device)`,
  `DeviceList::well_formed(max)`(항목 수·중복 검사, 서명 확인 전에), `DeviceList::with_entry(entry)`(항목 하나만 넣은 다음 판:
  새 기기를 승인하는 판·폐기 판), `DeviceList::heal(forged, at)`(폐기된 기기가 만든 판이 더한 기기만 폐기로 덧붙인 바로잡는
  판, 판 번호는 `MAX_FOLLOWED_VERSION = u64::MAX / 2`까지만 따라감). 판마다 만든 기기가 공동 서명하고
  (`DeviceList::sign_with(identity, device)` → `SignedDeviceList{cosig: Some(DeviceCosig)}`), 받는 쪽은
  `SignedDeviceList::classify(known) -> ListChange::{Authorized, Recovery(DeviceId), OtpMissing(DeviceId), Rejected}`로 아는 목록에
  비춰 가립니다(폐기된 기기가 신원 키로 기기를 더하지 못하게). 데몬은 `Authorized`만 받습니다. `Recovery`·`OtpMissing`은 해소기가 옛
  기록을 다시 적용할 때 쓰는 규칙으로 남았습니다(decisions.md "기기 연결").
- 없어진 계정 OTP의 흔적(decisions.md "OTP 제거"): `DeviceList.otp: Option<OtpKeyId>`(없으면 직렬화에서 빠져 서명 바이트가 예전과 같음)는
  서명 대상이라 남았고 다음 판이 그대로 이어받습니다. 옛 복구 판의 칸 `SignedDeviceList.otp_sig`, `SignedDeviceList::with_otp_sig(otp_key)`,
  `device::otp_signing_key(secret)`(서명 영역 `SignDomain::DeviceListOtp`, 확인은 `verify`)는 옛 기록 재적용용입니다: 아는 목록에 OTP 키가 있는데
  `otp_sig`가 없는 복구 판은 `OtpMissing`, OTP 키를 바꾼 복구 판은 `Rejected`(`KnownDevices::known_otp`). `SignedDeviceList`·
  `MemberDevices`의 이진 형식은 OTP 칸이 없으면 예전과 바이트가 같습니다(protocol.md "OTP 칸의 형식 호환").
- 기기 항목은 서명 공개키를 싣습니다: `DeviceEntry{device, box_key, sign_keys: DeviceSignKeys{ed_pk, pq_pk}, name, added_at,
  revoked_at}`. `DeviceEntry::keys_match()`(기기 ID = 실린 키의 키 ID)를 `well_formed`·`SignedDeviceList::verify`가 서명 확인
  전에 봅니다. `device::key_commit(box_key, sign_keys)`(BLAKE3 영역 분리)가 기기 키 약속이고, 공간 상태는 서명된 목록 대신
  간추린 기록 `MemberDevices{version, devices: Vec<DeviceRecord>, otp: Option<OtpKeyId>}`(`DeviceRecord{device, added_at, revoked_at,
  ed_pk, pq_hash, key_commit}`, 이름·키 본문 없음, `MemberDevices::from_list`)만 둡니다. 키 본문은 `DeviceKeys{device, box_key,
  sign_keys}`로 키 약속에 묶어 저장소의 기기 키 표에 둡니다. `extends`·`classify`는 `KnownDevices`(서명된 `DeviceList`와
  `MemberDevices` 둘 다 구현)에 비춰 보고, 같은 기기는 같은 키 약속이어야 이어받습니다(protocol.md "기기 목록"). 해소기가
  간추린 새 판을 아는 판에 비출 때는 `MemberDevices::extends(old)`·`MemberDevices::classify(cosigner, otp_signed, known)`(서명된
  목록의 것과 결과가 같음, `otp_signed`는 해소기 사실 `ListFacts.otp_signed`)를 씁니다.
- 키 ID는 키에서 나옵니다: `body::state_key_id(space, key) -> StateKeyId`, `body::group_root_id(space, group, r) -> RootKeyId`,
  `body::channel_root_id(space, channel, root) -> ChannelRootId`. 키 묶음의 키는 `SharedKey::{State{id, key}, Root{group, id,
  key}, ChannelRoot{channel, id, key}, Volatile{channel, generation, key}}`이고 `SharedKey::bound_to(space)`가 ID와 키가 맞는지
  봅니다(휘발성 키는 늘 참). 본문 `EventBody::KeyBackupSet{device: Option<DeviceId>}`, `EventBody::KeyUpdate(KeyUpdatePayload)`
  (`decode`가 조각 구조 검사 `validate`까지).
- 초대 가입 증명 `InviteProof`에는 가입 기기의 서명(`signed_by_joiner`, `joined_device`)이 붙습니다.
- 인증 거절 답 `YpResponse::Revoked{devices}`(폐기된 기기가 스스로 지우게).
- 새 기기 연결: 링크 `links::DeviceLink{v, user, token, contact, direct}`(`yangpa://device/<bs58>`, 앱 전용 주소, `direct`는 직접 스웜의
  `DirectContact{peer_id, addrs}`), 노드 간
  `YpRequest::DeviceLink(DeviceLinkRequest{token, entry})`(인증 전에도 받음)와 `DeviceLinkPoll{token}`, 답
  `YpResponse::DeviceLinkOpened{nonce}`·`DeviceLinkGrant(봉인)`, 봉인 안의 `DeviceLinkGrant{seed, devices, profile}`, 비교 번호
  `wire::device_link_code(token, nonce, entry)`(protocol.md "새 기기 연결").
- 후원자 확인 결과 나누기: `YpRequest::SupporterShare(SupporterShare{provider, login, active, valid_until})`(계정을 연결한 내
  기기가 인증된 내 다른 기기에게, 답 `Ok`, decisions.md "후원자 확인").
- 표시 상태: `YpRequest::Presence(PresenceShare{status: Online | Away | Invisible, hide_last_seen})`(인증된 친구·멤버 기기에게,
  답 `Ok`, protocol.md "표시 상태").
- 도전과제 뱃지(코어 `badge`): 서명 카드 `SignedBadgeCard{user, card: BadgeCard{version, earned, shown, founder?}, sig}`, 개발자
  계정의 원년멤버 증서 `FounderGrant{user, sig}`(`DEVELOPER`), 이름 검사 `has_emoji`. 노드 간 `YpRequest::Badges(Card | Grant |
  Own(OwnBadgeCounts))`(답 `Ok`), 상태 이벤트 `MemberBadges{card}`(101, `EventBody` 50번 변형)(protocol.md "뱃지 카드").
- 노드 간 `Social(Profile | Devices | Notes | Contact | Propose | ContactExpired)` 요청과 `SocialReply` 답: 프로필 교환, 기기
  목록 알림(연결한 새 기기 합류), 내 기기끼리 봉인한 개인 메모(protocol.md "기기 목록·프로필·개인 메모"), 링크용 주소 교환
  (`Contact{addrs, nodes}`·`SocialReply.addrs`·`.nodes`, 친구·내 기기와는 지금 노드 `NodeContact{peer_id, onion?}`도, protocol.md
  "피어 찾기와 위치 기록"), 임시 연락처의 친구 추가 요청(`Propose`, 답의 `SocialReply.friends`)과 만료 알림(`ContactExpired`). 내
  기기끼리는 `Devices{devices, spaces, contacts}`와 답의 같은 칸으로 공간·연락처 기록(`OwnContact`)을 맞춥니다(protocol.md "내 다른 기기로 전하기").
- 연락처 링크: 링크 사용 `RedeemFriendLink(FriendRedeem{token, auth, profile})`, 수락
  `FriendAccepted{token, space, events, profile?, contact?: ContactTerms{expires_at?}}`(`contact`가 있으면 임시 연락처, 없으면
  예전 친구 링크의 친구), 응답 `FriendPending`(protocol.md "연락처 링크와 친구").
- 상태 이벤트 `MemberProfile{nickname?, profile?}`: 작성자 자신의 크루 별명·서명 프로필(역할을 싣지 않음).
- 모더레이션: 권한 비트 `BAN_MEMBERS`(`ban`), 상태 이벤트 `MemberBan{user, reason, delete_since?}`(63)·`MemberUnban{user}`(64),
  `SpaceState::bans`(`BanInfo{reason, banned_by, banned_at}`), 운영진 기기에 봉인해 보내는 `YpRequest::Moderation(Report | Resolved)`
  (protocol.md "모더레이션").
- 보관 전용 노드(`archive`): 상태 이벤트 `ArchiveGrant{device, user, name, contact, private_channels}`(65)·`ArchiveRevoke{device}`(66),
  `SpaceState::archives`, 지정 링크 `ArchiveOfferLink`(`https://…#archive/…`), 권한자 기기가 서명한 멤버 명단
  `SignedArchiveRoster`, 노드 간 `YpRequest::Archive(Designate | Roster | BlobRefs | Prune | Release)`(protocol.md "보관 전용 노드").
- P2P 전용 채널: `model::ChannelTransport{Tor(기본), P2p}`, `SpaceState::channel_transport`·`is_p2p`, 상태 이벤트
  `ChannelTransportSet{channel, transport}`(67), Tor로 주고받는 참여 안내
  `YpRequest::P2pHello(P2pHello{peer_id, addrs, channels})`/`YpResponse::P2pHello`(주소 `MAX_P2P_ADDRS` 16개·채널
  `MAX_P2P_CHANNELS` 1024개까지)(protocol.md "P2P 전용 채널"). 새 노드 간 요청·응답은 열거형 뒤에 붙입니다(postcard 변형 번호):
  지금 `YpRequest`의 마지막은 멤버 프로필·카드 묻기 `GetMemberCards{space, users}`(그 앞은 새 기기 연결 `DeviceLink`·`DeviceLinkPoll`, 뱃지 `Badges`, 표시 상태 `Presence`, 후원자 확인 `SupporterShare`, 키 그룹 중계 `GetChannelRoots`·`GetGroupKeys`·
  `KeyTableHandover`·`GetStateKeys`·`GetKeyShard`, 체크포인트 `GetDeviceKeys`·`GetCheckpoint`, `GetGroupMembers`,
  `VolatileKeyWrap`, 채널 끝 묻기 `GetChannelTips`, 멤버 소개 `FindDevice{space, user, device}`, 멤버 노드 목록
  `GetMemberNodes{space}`, 놓친 가입 확인 `CheckJoin{spaces, auth}` 다음), `YpResponse`의
  마지막은 `MemberCards(Vec<MemberCard>)`(`DeviceLinkOpened{nonce}`·`DeviceLinkGrant(봉인)`, `SealedKeys`·`DeviceKeys`·`Checkpoint`·`ChannelTips`·`DeviceNode{peer_id, addrs}`·
  `MemberNodes(Vec<MemberNode>)` 다음, protocol.md "피어 찾기와 위치 기록"·"새 기기 연결").
- 역할 표시 설정: `model::RoleDisplay{visible_to, see_admins}`, `SpaceState::role_display`·`visible_roles`·`sees_admins`·`is_admin`,
  상태 이벤트 `RoleDisplaySet{role, display}`(68). 데몬 보기(`space.get`·`member.list`·`role.list`)만
  거릅니다(protocol.md "역할 표시 설정").
- 익명 채널: `SpaceState::anonymous_channels`·`is_anonymous`, 상태 이벤트 `ChannelAnonymousSet{channel, anonymous}`(69, `EventBody`
  38번 변형), 헤더 종류 `EventKind::AnonMessage`(7)·`AnonForumPost`(8)와 `base`·`is_anonymous`·`anonymized`, 본문 검사
  `EventBody::fits`. 저장소 `MessageRecord::anonymous`(이벤트 헤더 종류에서 읽음), API 보기는 `ANONYMOUS_USER`·`ANONYMOUS_DEVICE`로
  가림(protocol.md "익명 채널").
- 포럼 대문: `SpaceState::forum_featured`, 상태 이벤트 `ForumFeaturedSet{channel, post}`(71, `EventBody` 40번 변형).
  데몬 보기는 이 기기에 지우지 않은 게시글로 있을 때만 대문으로 보입니다(`ChannelView.featured_post`, `ForumPostView.featured`)
  (protocol.md "포럼 대문").
- 채널 첨부 한도: `SpaceState::attach_limits`·`attach_limit`, `model::MIN_ATTACH_LIMIT`(1MiB)·`valid_attach_limit`(0은 첨부 금지),
  상태 이벤트 `ChannelAttachLimitSet{channel, max_bytes: Option<u64>}`(72, `EventBody` 41번 변형). 데몬 보기 `ChannelView.attach_limit`
  (protocol.md "채널 첨부 한도").
- 채널 수정·삭제 금지: `SpaceState::edit_rules`(`model::EditRules{no_edit, no_delete}`), 상태 이벤트
  `ChannelEditRulesSet{channel, no_edit, no_delete}`(73, `EventBody` 42번 변형). `authorize_message`가 `Edit`·`Delete`·게시글
  제목·태그 변경을 거르고, 데몬 보기는 `ChannelView.no_edit`·`no_delete`(protocol.md "채널 수정·삭제 금지").
- 휘발성 세션 키 회전: `VolatileRotation(VolatileRotationBody{generation, parent, fresh, added, removed, recipients_hash, commit})`
  (키 레인 21, `EventBody` 46번 변형, `validate`가 모양 검사)와 직접 요청 `YpRequest::VolatileKeyWrap`, `keysched::{vol_join,
  vol_key_commit, vol_recipients_hash, vol_wrap_ad, vol_notice_key}`, 권한 `authorize_volatile_rotation`. 상시 참여 상한:
  `SpaceState::volatile_caps`·`volatile_cap`(`model::{DEFAULT,MIN,MAX}_VOLATILE_CAP` 200·2·2000), 상태 이벤트
  `ChannelVolatileCapSet{channel, cap: Option<u32>}`(78, 47번 변형), 데몬 보기 `ChannelView.volatile_cap`·`volatile_full`
  (protocol.md "휘발성 채널").
- 역할 순위: `Role::level`(관리자 권한이면 `model::ADMIN_LEVEL` 2³²를 더한 `rank`), `SpaceState::rank_of`는 u64(소유자 `u64::MAX`).
  해소기·데몬의 모든 순위 비교와 상태 해소 순서가 이것을 씁니다(protocol.md "크루"의 순위).
- 공간 상태에서 멤버 수만큼 커지는 표와 한도까지 채우면 큰 관리 설정은 지속 자료 구조입니다(`model::SharedMap` =
  `imbl::OrdMap`, 복제가 O(1)인 지속 B-트리; 초대는 `imbl::Vector`): `members: SharedMap<UserId, Arc<Member>>`,
  `devices: SharedMap<UserId, Arc<MemberDevices>>`, `bans: SharedMap<UserId, BanInfo>`, `channels: SharedMap<ChannelId,
  ChannelSettings>`, `role_display: SharedMap<RoleId, RoleDisplay>`, `invites: imbl::Vector<InviteState>`. 나머지 작은 표는
  `BTreeMap`. 멤버·기기 기록 값을 바꿀 때는 `Arc::make_mut`, 지속 맵에는 `values_mut`가 없어 고칠 키만 `get_mut`합니다.
  지속 맵의 직렬화는 예전 `BTreeMap`·`Vec`과 바이트까지 같습니다(`model::tests::shared_containers_encode_like_std`, 상태
  전체의 고정값은 `state_encoding_is_unchanged`). 해소기 스냅숏이 멤버 수·관리 설정 크기에 비례하지 않게 하려는 것입니다
  (performance.md). 두 판의 차이는 `model::DiffItem`(`imbl` `diff`, 나눠 쓰는 마디는 건너뜀)으로 보고, 저장(바뀐 행만)과
  색인(바뀐 멤버만)이 씁니다: `SpaceState::changed_members(newer) -> Vec<UserId>`, `active_devices() -> impl Iterator<Item =
  (&UserId, &DeviceRecord)>`(빌려 보는 반복자, 복사 없음), `member_active_devices(user)`, `members_with_devices() -> impl
  Iterator<Item = (&UserId, &Member, Option<&MemberDevices>)>`(두 표를 나란히 걸음, 사용자마다 찾지 않음),
  `permissions_in(user, Option<&Member>, Option<&ChannelSettings>)`(이미 쥔 항목으로 `channel_permissions`).
- 글 쓰는 중: 노드 간 일회성 요청 `YpRequest::Typing{space, channel}`(답 `Ok`, 저장하지 않음, protocol.md "글 쓰는 중").
- 키 그룹 형식(`keyupdate`, 본문 `EventBody::KeyUpdate`, protocol.md "키 그룹(`KeyUpdate`)"): `KeyGroupId::{crew, private}`
  (`ids::KeyGroupId`를 다시 내보냄: 비공개 그룹 갱신이 실릴 `StreamId::KeyGroup`과 같은 ID, 페이로드에는 길이 접두 없이
  16바이트 그대로),
  `NodeIdx{level, pos}`(잎에서 센 높이, 트리가 자라도 그대로), `Wrap`(56B), `RCt`, `SealedBox`, `LayoutOp::{Fill, Clear}`,
  `KeyUpdateKind::{Advance{joined}, Rotate{removed, joined}, Refresh, FullRekey{generation, removed, joined}, RRefresh}`
  (`is_full_rekey`, `same_variant`),
  `CrewRef{seq, root_commit}`(비공개·덮개 그룹이 기대는 크루 상태),
  `KeyUpdatePayload{group, seq, prev, kind, root_commit, prev_root_commit, crew, bundle_root, depth, salt, layout, wraps, sealed,
  r_ct, part, parts}`와 `encode`·`decode`(조각 하나의 목록 개수 한도를 디코딩 중에)·`validate`(조각)·`same_update`(조각끼리 같은
  갱신인지)·`validate_merged`(깊이에서
  정한 합친 한도)·`split(max_len) -> Result<Vec<_>>`(기기 목록까지 나눔, 조각이 `MAX_PARTS` 32개를 넘거나 한도를 넘으면 오류)·
  `merge(parts)`, 상수 `MAX_DEPTH` 17·`DIRECT_MAX` 256·`DIRECT_BACK` 160·`MAX_PAYLOAD_LEN` 960KiB·`PART_TARGET_LEN`·
  `MAX_PART_DEVICES`·`MAX_COVER_WRAPS`·`MAX_PARTS`. `SealedBox::recipients`·`sealed_recipients(envelope)`(봉인 봉투의 수신 기기를
  암호 없이 읽음, 해소기가 직접 인코딩 수신자를 검사하려고)와 봉투 형식 판 `SEALED_ENVELOPE_VERSION`.
- 상태 체크포인트(`checkpoint`, protocol.md "상태 체크포인트"): `CheckpointBody{space, seq, prev, heads, frontier_lamport,
  frontier, state_hash, user_root, users, device_root, devices, snapshot, snapshot_len, key_heads, valid_state_keys, owner,
  owner_devices, owner_proof, revoked, revocation_root, owner_chain, resolver_version, author, delegation, delegation_hash,
  signer_proofs}`와 `check_shape`,
  `SignedCheckpoint{body, owner_sig, device_sig}`(`sign_owner`·`sign_delegated`·`verify`·`id() -> CheckpointId`),
  `OwnershipCert`(앞 소유자 신원 + 이전한 기기, 바탕 체크포인트에 묶임, `OwnershipTransfer{new_owner, cert}`(`EventBody` 26번
  변형)에 실림)·`DelegationCert`(`hash`), `CheckpointPin{seq, id}`, `KeyHead{group, seq, root_commit}`(키 그룹 사슬의 머리, `CheckpointBody::key_head(group)`:
  크루 머리는 해소기가 자기 크루 사슬 끝과 견주고, 비공개·덮개 그룹 머리는 쓴 기기가 더함, protocol.md "키 그룹과 체크포인트"), `FrontierEntry`, 정렬 머클 트리
  `merkle::{leaf, root, levels, range_proof, verify_range, verify_absence}`와 `DeviceLeafProof`, `user_value`·`device_key`·
  `device_value`·`revocation_root`·`state_hash`, 상수 `RESOLVER_VERSION`·`FRONTIER_DEPTH`(32)·`MAX_FRONTIER`·
  `MAX_VALID_STATE_KEYS`(64)·`MAX_KEY_HEADS`·`MAX_SNAPSHOT_LEN`(32MiB)·`MAX_CHAIN_PER_REPLY`. 상태 이벤트
  `StateCheckpoint`(74, `EventBody` 43번 변형)·`CheckpointDelegation`(76, 44번)·`CheckpointDispute{checkpoint, seq, state_hash}`
  (77, 마지막 변형 45). 노드 간 `GetDeviceKeys{space, devices}`(512개까지) → `DeviceKeys([DeviceKeysItem{keys, proof}])`,
  `GetCheckpoint{space, from_seq}` → `Checkpoint(CheckpointReply{head, chain, event, manifest})`. `InviteLink`는 2판으로
  `checkpoint: Option<CheckpointPin>`이 붙습니다. 보관 명단 차이 `archive::{ArchiveRosterDelta, SignedArchiveRosterDelta,
  roster_hash, MAX_DELTA_CHANGES, MAX_DELTAS}`와 `ArchiveRequest::RosterDelta`. 서명 영역 `checkpoint`·`checkpoint-device`·
  `checkpoint-delegation`·`ownership-cert`·`archive-roster-delta`.
- 서명 영역 `SignDomain::KeyTable`(`yangpa/v3/key-table`): 확정 기기 넘기기 표(`yangpa_crypto::keytree::seal_handover`).

### yangpa-crypto

```rust
// aead.rs — XChaCha20-Poly1305, 24바이트 무작위 논스를 암호문 앞에 붙임
pub fn seal(key: &Secret32, ad: &[u8], plaintext: &[u8]) -> Vec<u8>;
pub fn open(key: &Secret32, ad: &[u8], ciphertext: &[u8]) -> Result<Zeroizing<Vec<u8>>, CryptoError>;
// pad.rs — 길이 누출을 줄이는 Padmé 패딩(4바이트 길이 접두 + 0 채움)
pub fn pad(data: &[u8]) -> Vec<u8>;
pub fn unpad(data: &[u8]) -> Result<&[u8], CryptoError>;
// kdf.rs — HKDF-SHA256
pub fn derive(ikm: &[u8], salt: &[u8], info: &[u8]) -> Secret32;
// body.rs — 이벤트 본문 암·복호화(encode → zl::pack → pad → seal, AD = header.aead_ad()). 예전 판(압축 없음)도 읽음
pub fn encrypt_body(key: &Secret32, header: &EventHeader, body: &EventBody) -> Result<Vec<u8>, CryptoError>;
pub fn decrypt_body(key: &Secret32, header: &EventHeader, ciphertext: &[u8]) -> Result<EventBody, CryptoError>;
// sealed.rs — 다중 수신자 봉인 상자(X-Wing(X25519 + ML-KEM-768) + HKDF + XChaCha20-Poly1305)
pub struct BoxKeypair;            // X-Wing 시드 32바이트, Zeroize. public() -> BoxPublicKey(1216바이트, yangpa-core device.rs)
pub fn seal_to(recipients: &[(DeviceId, BoxPublicKey)], ad: &[u8], plaintext: &[u8]) -> Vec<u8>;
pub fn open_sealed(me: &DeviceId, kp: &BoxKeypair, ad: &[u8], envelope: &[u8]) -> Result<Zeroizing<Vec<u8>>, CryptoError>;
pub fn sealed_recipients(envelope: &[u8]) -> Result<Vec<DeviceId>, CryptoError>; // 복호화 없이(yangpa_core::keyupdate::sealed_recipients와 같은 읽기)
// blob.rs — 조각 단위 AEAD(STREAM 방식, 조각 번호를 논스·AD에 묶음)
pub fn encrypt_blob(key: &Secret32, plaintext: &[u8], chunk_size: u32) -> (Vec<u8>, BlobManifest);
pub fn decrypt_blob_chunk(key: &Secret32, manifest: &BlobManifest, index: u32, chunk: &[u8]) -> Result<Vec<u8>, CryptoError>;
// blobpack.rs — 블롭 평문의 압축 머리글(14바이트: "YPZL", 판, 방식(그대로·OpenZL), 내용 길이) + 몸통
pub fn pack_blob(content: &[u8]) -> Zeroizing<Vec<u8>>;                                 // 메모리의 내용 → 블롭 평문
pub fn packed_source<R: Read>(r: R, len: u64) -> Result<(PackedSource<R>, u64), CryptoError>; // BlobEncryptor에 넣을 읽기기와 평문 길이
pub struct BlobReader<'m, F>;     // F: FnMut(조각 번호) -> Result<평문 조각, E>. 머리글을 보고 범위 읽기·전체 읽기(압축본은 풀어서)
impl BlobReader { fn open(&BlobManifest, F) -> Result<Self, ReadError<E>>; fn len(&self) -> u64 /*내용 길이*/;
                  fn read(&mut self, offset: u64, len: u64) -> Result<Zeroizing<Vec<u8>>, ReadError<E>>;
                  fn for_each(&mut self, sink: impl FnMut(&[u8]) -> Result<(), E>) -> Result<(), ReadError<E>>; }
// identity.rs — 신원 키·기기 키, 안전 번호. 시드 하나에서 신원 키 전체가 나옴
pub struct Identity { pub signing: SigningKeypair }
impl Identity { fn generate(); fn from_seed(&[u8; 32]); fn user_id(&self) -> UserId; }
pub struct DeviceKeys { pub signing: SigningKeypair, pub boxkey: BoxKeypair }
impl DeviceKeys { fn generate(); fn device_id(&self) -> DeviceId; fn to_bytes(&self) -> Zeroizing<Vec<u8>>; fn from_bytes(&[u8]); }
// locked.rs — 잠긴 메모리(mlock/VirtualLock)의 키 보관소. 해제 시 0으로 덮어씀.
pub struct LockedKeyring;         // 키 여러 개를 잠긴 페이지 풀에 보관(OS 잠금 한도 절약)
impl LockedKeyring { fn new(capacity: usize) -> Result<Self>; fn insert(&mut self, id: [u8;16], key: &Secret32) -> Result<()>;
                     fn with_key<R>(&self, id: &[u8;16], f: impl FnOnce(&Secret32) -> R) -> Option<R>; fn remove(&mut self, id: &[u8;16]); fn is_locked(&self) -> bool; }
// keysched.rs — 키 일정 v3(T·R에서): state_key(R), public_root(R, 공간, 채널), private_root(T, 공간, 채널), record_key(뿌리),
//               channel_root_id·group_root_id(계산은 yangpa_core::body 하나), joined_hash, advance(T, 그룹, seq, 가입 해시),
//               root_commit(공간, 그룹, seq, T, R), ct_eq, seal_r/open_r
// keytree.rs — 키 그룹 계산(protocol.md "키 그룹(`KeyUpdate`)"). 직접 인코딩 전체 재키도 FullRekey(깊이 0, 세대 그대로). 상태는 메모리에만, 밖으로는 seal_handover로만.
pub enum GroupKind { Crew, Private, Cover }
pub trait BoxKeys { fn box_key(&self, d: &DeviceId) -> Option<&BoxPublicKey>; }   // HashMap·BTreeMap<DeviceId, BoxPublicKey>
pub struct CommitCtx<'a> { pub boxes: &'a dyn BoxKeys, pub crew: Option<&'a KeyTree> }
pub struct KeyUpdateOutput { pub payload: KeyUpdatePayload, pub bundles: Vec<DeviceBundle> }
// 기기 하나에게 가는 봉인 묶음과 payload.bundle_root 머클 증명. encode/decode(16KiB·증명 17칸 한도), verify(&root)
pub struct DeviceBundle { pub device: DeviceId, pub envelope: Vec<u8>, pub index: u32, pub proof: Vec<[u8; 32]> }
pub fn by_user(bundles, user_of) -> BTreeMap<UserId, Vec<DeviceBundle>>;          // 전체 재키 블롭 조각·Inbox 묶기
#[derive(Clone)] pub struct KeyTree;   // 확정 기기 쪽 크루·비공개 그룹. 받아들여질 때까지 작업 전 clone을 남겨 둠
impl KeyTree { fn crew(space); fn private(space, group); fn recover(space, group, kind, ChainTip, devices);
               fn update(&mut self, removed, joined, &CommitCtx) -> Result<KeyUpdateOutput, KeyUpdateError>;
               fn refresh_root(&mut self, &CommitCtx); fn refresh_r(&mut self, &CommitCtx); fn full_rekey(&mut self, removed, joined, &CommitCtx);
               fn holders(&self) -> &BTreeSet<DeviceId>; fn add_holders(&mut self, devices);   // 크루: 표 보유 기기
               fn needs_rebuild(&self, crew: &KeyTree) -> bool; fn set_last_event(&mut self, EventId); fn tip(&self) -> ChainTip;
               fn seq/t/r/root_commit/generation/depth/devices/contains/slot_of/cover(&excluded) … }
#[derive(Clone)] pub struct CoverGroup; // "몇 기기만 빼고 크루 전체": new, recover, update(&excluded, &cx) -> Result<Option<_>>,
                                       // refresh(&excluded, &cx), refresh_r(&excluded, &cx), tip
pub struct MemberKeys; // 멤버 쪽: join(space, group, kind, device, &payload, &MemberCtx), apply(&self, &payload, &MemberCtx) -> 새 값,
                       // state_key, public_root, private_root, t, r, seq, depth, slot …
// crew: payload.crew와 같은 순번까지 적용한 크루 상태(비공개·덮개). 다르면 CrewMismatch
pub struct MemberCtx<'a> { pub box_keys: Option<&'a BoxKeypair>, pub bundle: Option<&'a DeviceBundle>, pub crew: Option<&'a MemberKeys> }
// 보낸 확정 기기가 서명(SignDomain::KeyTable). 받는 쪽은 from(해소기가 정한 보낸 기기)의 서명과 표가 스스로 맞는지 확인
pub fn seal_handover(trees: &[&KeyTree], covers: &[&CoverGroup], to: &[(DeviceId, BoxPublicKey)], signer: &SigningKeypair) -> Result<SealedHandover>;
pub fn open_handover(me: &DeviceId, kp: &BoxKeypair, h: &SealedHandover, from: &DeviceId) -> Result<(Vec<KeyTree>, Vec<CoverGroup>)>;
// 오류: …, RootCommitMismatch, BadBundle, CrewMismatch{want, have}, HolderNeedsFullRekey(DeviceId), BadHandover(&str)
```

### yangpa-zl

OpenZL(`openzl-sys` 0.3.0, C 소스를 함께 빌드. 윈도는 clang-cl 필요)의 안전한 래퍼. 스레드마다 압축·풀기 문맥을 다시 씁니다.

```rust
pub const TAG_RAW: u8 = 0xF0; pub const TAG_ZL: u8 = 0xF1; pub const FORMAT_VERSION: i32 = 27;
pub fn pack(data: &[u8]) -> Zeroizing<Vec<u8>>;                 // 표시 바이트 + (줄면 OpenZL, 아니면 그대로)
pub fn unpack(data: &[u8], max: usize) -> Result<Zeroizing<Vec<u8>>, ZlError>; // 풀린 크기 > max면 풀지 않고 거절
pub fn unpacked_size(data: &[u8]) -> Result<usize, ZlError>;   // 풀기 전에 예산을 잡을 때
pub fn is_packed(data: &[u8]) -> bool;                          // 예전 형식 가려내기
pub fn compress(src: &[u8]) -> Result<Zeroizing<Vec<u8>>, ZlError>; pub fn decompress(frame: &[u8], max: usize) -> …;
```

### yangpa-store

```rust
pub struct Store;   // SQLCipher 연결(WAL, secure_delete=ON, foreign_keys=ON) + 블롭 디렉터리. Send + Sync.
impl Store {
    fn open(dir: &Path, db_key: &Secret32) -> Result<Store>;
    fn open_temp() -> Result<(Store, TempDir)>;           // 테스트용
    // 이벤트(디스크 등급만). 휘발성 채널 이벤트는 타입 단위로 받지 않음(아래 VolatileStore).
    fn insert_events(&self, evs: &[PersistentEvent]) -> Result<Vec<InsertOutcome>>;  // 한 트랜잭션으로 묶음
    fn get_event(&self, id: &EventId) -> Result<Option<SignedEvent>>;   // 본문은 메시지 키로 풀어서 원래 암호문 반환
    fn has_events(&self, ids: &[EventId]) -> Result<Vec<bool>>;
    fn heads(&self, space: &SpaceId, stream: StreamId) -> Result<Vec<EventId>>;
    fn max_lamport(&self, space: &SpaceId) -> Result<u64>;
    fn ids_in_range(&self, space, stream, range: LamportRange, limit) -> Result<Vec<(u64, EventId)>>;
    fn fingerprints(&self, space, stream, ranges: &[LamportRange]) -> Result<Vec<RangeFingerprint>>;
    fn missing_parents(&self, space, stream) -> Result<Vec<EventId>>;    // '받지 않은 기록' 구간
    fn events_by_kind(&self, space, lanes: &[Lane]) -> Result<Vec<SignedEvent>>; // 상태 해소 입력
    fn events_by_lane_from(&self, space, lanes: &[Lane], from: u64) -> Result<Vec<SignedEvent>>; // 논리 시계 from 이상만(체크포인트 경계 위로 다시 세우기)
    fn forget_missing(&self, space, stream, ids: &[EventId]) -> Result<usize>; // 앞으로 받지 않을 빠진 부모(경계 아래)를 기록에서 지움
    fn erase_body(&self, id: &EventId) -> Result<()>;                   // 메시지 키 폐기(암호학적 삭제)
    fn wipe_all(&self, keep_secrets: &[&str], keep_settings: &[&str]) -> Result<()>; // 폐기된 기기: 남길 비밀·설정 접두어 말고 모든 표를 한 트랜잭션으로, 블롭 파일(못 지우면 다음 열 때)을 지우고 WAL 비움
    // 복호화한 메시지 보기(메시지 키로 봉인해 저장, 목록 조회가 빠름)
    fn put_message_view(&self, v: &MessageRecord) -> Result<()>;
    fn list_messages(&self, space, channel, before: Option<(u64, EventId)>, limit: u32) -> Result<Vec<MessageRecord>>;
    fn get_messages(&self, ids: &[EventId], viewer) -> Result<Vec<MessageRecord>>;  // 주어진 순서대로(검색 결과 한 쪽)
    // 메시지 검색(search.rs): 첫 검색에서 보기를 한 번 복호화해 RAM 색인(정규화한 글을 이어 붙인 버퍼)을 만들고, 쓰기 연결의
    // 변경 훅(update_hook)이 알린 messages 행만 다시 읽어 맞춤. 찾기는 memchr memmem을 버퍼 조각마다 스레드로, 쪽은
    // 선택 알고리즘으로 그 구간만 정렬. accept로 부르는 쪽이 권한·숨김을 거르고, total은 거른 뒤의 수. 색인은 디스크에 안 씀
    fn search_messages(&self, query: &str, filter: SearchFilter, accept: impl FnMut(&SearchHit) -> bool, offset: usize, limit: usize) -> Result<SearchPage>;
    fn reset_search_index(&self);   // 표를 통째로 지운 뒤(wipe_all이 부름)
    // 키
    fn put_key(&self, kind: KeyKind, id: &[u8], scope: &[u8], key: &Secret32) -> Result<()>;
    fn get_key(&self, kind: KeyKind, id: &[u8]) -> Result<Option<Secret32>>;
    fn put_key_entry(&self, e: &KeyEntry) -> Result<bool>;             // 상태 키·채널 뿌리 행(먼저 온 것이 이김, 새로 넣었으면 true)
    fn insert_events_with_views(&self, evs: &[PersistentEvent], views: &[MessageRecord]) -> Result<Vec<InsertOutcome>>;  // 이벤트와 보기 행을 한 트랜잭션으로
    fn mark_message_deleted(&self, space, target, at) -> Result<bool>;  // 메시지 레인이 아닌 이벤트는 거부
    fn messages_by_author_between(&self, space, user, since, until) -> Result<Vec<(EventId, ChannelId)>>;  // 밴의 최근 메시지 삭제(끝 = 밴 시각)
    fn unread_counts_excluding(&self, space, me, excluded: &[UserId]) -> Result<…>;  // 차단한 사람 빼고 안 읽은 수(제외 목록 길이 상관없음)
    // 공간 상태(5판): 큰 표를 뺀 머리(spaces.head)와 멤버·기기 기록·밴·채널을 열쇠마다 한 행으로, 바뀐 행만 씀
    fn apply_state_delta(&self, space: &SpaceId, delta: &StateDelta) -> Result<()>;  // StateDelta::between(old, new)
    fn put_space_state(&self, space: &SpaceId, state: &SpaceState) -> Result<()>;     // 그 공간 행을 지우고 통째로(기준이 없을 때)
    fn load_space_state(&self, space: &SpaceId) -> Result<Option<SpaceState>>;      // 행에서 똑같이 다시 지음
    // 기기 키 표(키 약속 → 기기 공개키, 내용 주소). 행 = (키 약속, 넣은 공간): 공간을 지우면 그 공간 몫만 지움
    fn insert_events_with_device_keys(&self, evs: &[PersistentEvent], space: &SpaceId, keys: &[DeviceKeys]) -> Result<Vec<InsertOutcome>>;  // 받은 상태 이벤트와 실린 키를 한 트랜잭션으로
    fn put_device_keys(&self, space: &SpaceId, keys: &[DeviceKeys]) -> Result<usize>;  // 기기 ID와 서명 키가 안 맞으면 거름
    fn device_keys(&self, commits: &[[u8; 32]]) -> Result<HashMap<[u8; 32], DeviceKeys>>;  // 읽을 때 약속을 다시 셈
    fn device_key_commits(&self, space: &SpaceId) -> Result<HashSet<[u8; 32]>>;  // 그 공간 몫 약속(색인 `device_keys_space`만 읽음)
    fn identified_peers(&self) -> Result<Vec<PeerRecord>>;  // 기기가 묶인 노드 기록(공간 멤버 노드를 한 번 훑어 고름)
    // 범용 키-값과 도메인 테이블: identity, friends, friend_links, friend_requests, device_lists,
    // spaces·space_members·space_bans·space_channels, device_keys, profiles, notes, settings, api_tokens,
    // audit_log, blobs, invites_used …
}
pub struct StateDelta;    // 상태 두 판 사이 바뀐 행(between, rows, value_bytes, is_empty)
pub struct BlobStore;  // <dir>/blobs/ab/cdef… (이미 암호화된 블롭 조각을 해시 이름으로 저장)
pub struct VolatileStore;  // RAM 전용. 암호문만 보관, 키는 LockedKeyring. Persist 트레잇을 구현하지 않음.
                           // 세대 번호가 무작위라 보낼 세대는 make_current로 정함(정리에서 지킴).
                           // 휘발성 첨부 블롭(stage_blob → claim_blob/remove_staged, register_blob → write_blob_chunk,
                           // trim_blob_reservation): 메시지 하나에 묶여 그 메시지와 함께 지워지고, 받는 동안 크기만큼
                           // RAM 예산(VolatileLimits::max_blob_bytes)을 미리 잡음(쉬는 동안은 가진 만큼).
// 누출 검사: Store::scan_for_marker(복호화한 DB·봉인 열·데이터 경로의 모든 파일), Store::scan_blobs_for_marker(키를 아는
// 디스크 블롭의 평문), 데이터 경로 밖 파일(로그)은 이것으로
pub fn file_contains_marker(path: &Path, marker: &[u8]) -> io::Result<bool>;
```

휘발성 분리는 타입으로 강제합니다. 디스크 저장 API는 `PersistentEvent`만 받고, `PersistentEvent::new`는 채널이
휘발성이면 실패합니다. `VolatileEvent`는 `Serialize`를 구현하지 않고 디스크 저장 함수 어디에도 들어갈 수 없습니다.

### yangpa-sync (순수 로직, 입출력 없음)

```rust
pub mod dag;      // 스트림별 머리(heads)·빠진 부모 추적. 빠진 부모는 오류가 아니라 '받지 않은 기록' 구간.
pub mod reconcile;// 논리 시계 구간 지문(XOR+개수) 비교로 빠진 이벤트만 찾기. split_range, Reconciler 상태 기계.
pub mod state;    // 상태 해소: 상태 이벤트를 결정론적 순서로 적용하며 권한 검사.
                  // pub fn resolve(events: &[DecodedEvent]) -> Resolved { state: Option<SpaceState>, applied, rejected: Vec<(EventId, AuthError)>, key_events, member_since, genesis, keys: Option<KeyView> }
                  //   key_events: 데몬이 처리할 키 이벤트(적용 순서): 다 모여 받아들인 크루 KeyUpdate(마지막 조각), KeyBackupSet
                  //   크루 키 사슬(state/machine/keys.rs, protocol.md "키 그룹"): KeyView { crew: CrewKeys, owners: 기기 → 사용자 }
                  //     CrewKeys { seq, head, root_commit, generation, depth, members, joined_at, debt, lost_by, last_rekey, holders, backup, rekey_required, tip_part }
                  //     CrewKeys::parts_of(seq) / Resolver::crew_parts(seq) -> Option<Arc<[EventId]>>  // 받아들인 갱신의 조각 ID(최근 64순번)
                  //   Resolver::point_at(anchor) -> Option<(KeyPoint, bool)>  // state_at과 같되 키 사슬까지, KeyPoint { state, keys }, bool = 빈 구간을 끼고 셈
                  //   Resolver::gap_epoch() -> u64  // 아는 이벤트의 조상 집합이 바뀔 때마다 오름(빈 구간 판정 다시 하기)
                  //   Resolver::point() / key_view()  // 지금 것
                  //   Resolver::includes(later, earlier) -> bool  // earlier가 모두 later 가운데 하나이거나 그 조상인지(해소기가 아는 DAG)
                  //   Resolver::joins_reachable(parents, devices) -> Vec<DeviceId>  // 활성으로 만든 이벤트가 parents에서 닿는 가입 기기(확정 기기가 크루 갱신 부모를 고를 때)
                  // pub struct GroupChain  // 키 그룹 스트림(비공개·덮개)의 KeyUpdate 사슬. 데몬이 그룹마다 하나(keygroup 멤버 쪽)
                  //   fn new(space, group); fn insert(&mut self, id, &header, &KeyUpdatePayload, &dyn KeyPoints) -> Result<ChainChange, AuthError>
                  //   fn rejudge(&mut self, &dyn KeyPoints) -> ChainChange  // 지금 상태가 바뀌었거나 기다리던 기준점을 받은 뒤
                  //   fn tip() -> GroupTip { seq, head, root_commit, members, crew_seq, fresh_crew, members_known }; fn winners(); fn parts(head); fn verdict(head)
                  //   fn from_head(space, &KeyHead) -> Option<Self>  // 체크포인트 키 머리에서 시작(머리의 HeadChain으로 바닥을 세움, 바닥 멤버는 요약만 앎)
                  //   fn apply_head(&KeyHead, &dyn KeyPoints) -> bool  // 바닥을 머리 마디로 바꿈, 머리 뒤 늦은 갱신은 버림
                  //   fn learn_members(seq, &root_commit, &[DeviceId]) -> bool  // GetGroupMembers로 받은 바닥 멤버(머리의 멤버 요약과 맞을 때만)
                  //   fn members_at(seq, &root_commit) -> Option<Vec<DeviceId>>; fn key_head() -> Option<KeyHead>  // 중계용 멤버, 체크포인트에 실을 머리
                  //   fn saved_base() -> ChainBase; fn from_saved(space, group, ChainBase); fn prune_covered(seq)  // 재시작용 바닥(체크포인트 머리까지 접은 것)
                  //   fn send_tip() -> Option<GroupTip>  // 지금 쓸 수 있는 끝(작성 기기가 지금도 표 보유 자격). 없으면 보낼 키 없음
                  //   fn winning_at(seq, &root_commit) -> Option<GroupTip>  // 아직 쓸 수 없는 뒷부분까지 본 이긴 사슬의 그 상태(넘겨받은 새 확정 기기가 이음)
                  //   fn would_accept(user, author, anchor, lamport, &payload, &dyn KeyPoints) -> Result<(), AuthError>  // 확정 기기가 내기 전 미리 판정
                  //   fn take_equivocations() -> Vec<Equivocation { seq, author, heads: [EventId; 2] }>  // 같은 작성 기기의 같은 순번 두 판
                  //   ChainChange { accepted, dropped }  // 쓸 수 있는 사슬의 변화(늦게 온 후보가 이기면 dropped)
                  //   trait KeyPoints { at(anchor) -> Option<(KeyPoint, bool)>; now() -> Option<&SpaceState>; includes(later, earlier); gap_epoch();
                  //     group_floor(group) -> u64 (머리가 있는 그룹만 바탕 F); below_floor(id); knows(id) }  // Resolver가 구현
                  //   상수 MAX_FORKS 8(같은 순번 작성 기기마다), MAX_SEQ_CANDS 64(같은 순번 모두), MAX_AUTHOR_ORPHANS 64·MAX_ORPHANS 1024
                  //   (머리보다 먼저 온 조각), KEEP_SEQS 64(남기는 사슬). AuthError::AnchorUnknown: 기준점을 받은 뒤 다시 넣음
                  //   UpdateKind: 갱신 종류(목록 없이), 해소기·사슬이 간추린 KeyFacts에 둠
                  // pub struct Resolver  // 데몬이 공간마다 하나. 결과는 늘 같은 항목으로 `resolve`한 것과 같음(protocol.md "상태 해소 순서")
                  //   fn with_genesis(genesis) / with_genesis_limits(genesis, Limits { max_members })  // 한도 바꾸기는 규모 시뮬레이션용
                  //   fn insert(&mut self, events, links) -> ResolveUpdate  // 새 항목이 머리를 다 덮으면 이어서 처리(증분),
                  //       아니면 처리 기록의 스냅숏에서 다시 처리(재생), 기록을 못 쓰면 처음부터(rebuild). stats()가 셈
                  //   스냅숏: 처리 1024항목마다 Machine 복제(지속 맵이라 O(1)), 16개까지 끝에서 멀수록 성기게 솎음.
                  //       재생 위치가 기록 끝이면 스냅숏 없이 새 항목만 처리. ResolveUpdate.full은 증분이 아니었다는 뜻(재생 포함)
                  //   들고 있는 이벤트(state/facts.rs `Stored`): 가입·기기 목록 갱신은 넣을 때 서명·모양을 한 번
                  //       확인하고 결과와 간추린 기록(JoinFacts·DevicesFacts·ListFacts·InviteFacts)만 남김(본문 14KB 남짓을 버림)
                  // pub fn authorize_message(state: &SpaceState, header: &EventHeader, body: &EventBody, target_author: Option<UserId>) -> Result<(), AuthError>
                  // pub fn state_key_handover_only(state: &SpaceState, header: &EventHeader) -> bool  // 상태 키 묶음이 확정 기기 인계 예외로만 통과(받는 쪽은 지금 키로 쓰지 않음)
                  //   Resolver::state_at(anchor, allow_gaps) -> AnchorState::{Ready{state: Option<Box<SpaceState>>, gaps}, Incomplete{unknown, undecoded}}
                  //       // 기준점과 그 조상만으로 해소한 상태(부분 전달 이벤트 검사용). 범위가 있는 이벤트는 해소기에 넣지 않음.
                  //       // 본문 없이 헤더만 받은 링크(LinkEvent::without_body)는 빈 구간이 아님(from_header는 키를 기다리는 것)
                  //   Resolver::recent_applied(n) -> Vec<EventId>  // 마지막으로 적용한 상태 이벤트(작성 쪽 기준점 고르기)
                  //   Resolver::awaiting_body(id) -> bool  // 본문을 기다리는 링크인지
                  // pub fn authorize_scoped(state: &SpaceState, space: &SpaceId, header: &EventHeader) -> Result<(), AuthError>  // 받은편지함·키 그룹 작성 자격(데몬이 기준점 상태와 지금 상태로 부름)
                  //   체크포인트(protocol.md "상태 체크포인트"): 경계는 Resolver 안(차례로 받아들이기, 바탕 앞 항목은 RAM에서 버림)
                  //   Resolver::from_checkpoint(genesis, checkpoint, snapshot) / from_checkpoint_limits  // 새 멤버·재시작
                  //   Resolver::checkpoint_draft(current_key) -> CheckpointDraft  // 쓰는 기기: 머리·경계·해시·트리·스냅숏 평문
                  //   Resolver::{base, base_snapshot, base_state, device_proofs, since_base, floor, is_retired_state_key,
                  //       state_keys_in_use, delegation, key_heads, head_lamports, has_missing, waits_for}
                  //   ResolveUpdate.{checkpoints, conflicts, needs_update}, AuthError::{LateEvent, BelowCheckpoint, RetiredStateKey,
                  //       StaleCheckpoint, InvalidCheckpoint, NewerResolver, DuplicateDispute}
pub mod checkpoint; // 새 멤버의 사슬 검증: validate_chain(pin, chain, snapshot)(자격은 늘 앞 체크포인트로),
                  // 두 번째 출처 견주기 compare_heads(mine, space, head, chain) -> HeadAgreement{Same, Extends, Conflict, Unknown},
                  // 기기 키 답 거르기 accept_device_keys(item, known, tree), DeviceTree, DeviceProofs(답하는 쪽 잎 증명)
pub mod policy;   // SeederPolicy, PeerPolicy, DataType, 요청 처리 5단계 검사, TokenBucket(대역폭),
                  // may_read_scoped(state, space, user, device, stream, inbox_relay) -> Option<bool>  // 부분 전달 스트림을 내줄지(아니면 빈 스트림처럼 답함).
                  //     inbox_relay: 요청 기기가 그 받은편지함의 전달자(데몬 crews::is_inbox_relay)면 내줌
pub mod swarm;    // ChunkScheduler: 희귀 조각 우선, 피어별 동시 요청 상한, 시간 초과·재시도, 실패 피어 점수
pub mod priority; // 요청 순서: 상태 → 보고 있는 채널 최근 → 우선순위별 첨부 → 과거 기록 지연 로드
pub mod score;    // 피어 점수(서명·멤버십·권한·빈도 위반 시 감점, 임계 이하면 연결 해제)
pub mod archive;  // 보관 전용 노드: 보관 범위(archived_channels), 명단 만들기·받기(build_roster, accept_roster),
                  // 바탕 명단 위 차이 받기(accept_delta -> DeltaVerdict::{Accepted, Unchanged, Expired, Refused}),
                  // 멤버(시더)의 보관 노드 요청 검사(check_archive_seed), 보관 노드의 내주기 검사(check_archive_serve)
```

### yangpa-net

```rust
pub struct NetConfig { mode: NetMode /*Normal(직접 연결)|Tor*/, listen: Vec<Multiaddr>, bootstrap: Vec<Multiaddr>,
                       keypair: libp2p::identity::Keypair /*스웜마다 다른 네트워크 키*/, mdns: bool, relay_server: bool,
                       max_connections: u32, data_dir: PathBuf /*Arti 캐시*/, kad_mode: KadMode,
                       external_addrs: Vec<Multiaddr>, request_timeout: Duration, onion_port: u16,
                       upnp: bool /*직접 연결 스웜만: 공유기 포트 열기(libp2p-upnp), 기본 끔*/,
                       onion_listen: bool /*Tor 스웜이 onion 서비스로 받을지(기본 켬, 끄면 나가기만 하는 출구 스웜)*/,
                       tor_share: Option<TorShare> /*다른 스웜의 Arti를 함께 씀(Tor 스웜만)*/,
                       tor_isolated: bool /*Tor 스웜만: 연결마다 Arti 회로를 따로(같은 onion 주소로 걸어도)*/,
                       sticky_relays: Vec<StickyRelay> /*지난번 릴레이를 먼저 다시 예약. 데몬은 이제 채우지 않음*/,
                       traffic: Traffic /*요청·응답 바이트 계수기. 여러 스웜에 같은 것을 넘기면 함께 셈(전송 속도 표시)*/ }
#[derive(Clone, Default)] pub struct Traffic;   // fn totals(&self) -> (보낸 바이트, 받은 바이트), 코덱이 더함
pub struct StickyRelay { peer_id: PeerId, addrs: Vec<Multiaddr> /*릴레이 자체 주소, 최대 2개*/, public: bool }
pub fn spawn(cfg: NetConfig) -> Result<(NetHandle, mpsc::Receiver<NetEvent>)>;
#[derive(Clone)] pub struct NetHandle;
impl NetHandle {
    async fn request(&self, peer: PeerId, req: YpRequest) -> Result<YpResponse, NetError>;
    // 통화 미디어 프레임(`media.rs`, `/yangpa/media/1`): 기다리지 않고, 밀리면 오래된 것부터 버림. 이미 연결된 피어에게만.
    fn send_media(&self, peer: PeerId, frame: Vec<u8>);
    async fn dial(&self, peer: PeerId, addrs: Vec<Multiaddr>) -> Result<(), NetError>;
    async fn dial_as_listener(&self, peer: PeerId, addrs: Vec<Multiaddr>) -> Result<(), NetError>;
        // 홀펀칭의 받는 쪽(QUIC): 상대가 같은 때 `dial`로 거는 동안 수신 소켓에서 상대 주소로 보내 공유기 길을 엶. 시각은 부르는 쪽이 맞춤
    async fn find_peer(&self, peer: PeerId) -> Result<Vec<Multiaddr>, NetError>;   // Kademlia
    async fn put_record(&self, key: Vec<u8>, value: Vec<u8>) -> Result<(), NetError>; // 위치 기록(암호화된 값)
    async fn get_record(&self, key: Vec<u8>) -> Result<Vec<Vec<u8>>, NetError>;
    // 토픽 = wire::GossipTopic::{Crew(space), Group(space, group)}(이름 yp2/<hex>). 출처 없는(Anonymous) 메시지만.
    fn subscribe(&self, topic: GossipTopic); fn unsubscribe(&self, topic: GossipTopic);
    fn publish(&self, topic: GossipTopic, msg: GossipMessage);
    async fn try_publish(&self, topic: GossipTopic, msg: GossipMessage) -> Result<(), NetError>; // 받을 피어가 없으면 NoPeers
    fn report_gossip(&self, msg_id: MessageId, from: PeerId, accept: GossipVerdict);
    fn set_allowed_peers(&self, topic: GossipTopic, peers: Vec<PeerId>);   // 닫힌 스웜: 토픽마다 멤버 노드만 메시·전달
    fn penalize(&self, peer: PeerId, reason: &str);
    fn traffic(&self) -> Traffic;           // 이 스웜(과 같은 계수기를 받은 스웜)의 누적 바이트
    async fn status(&self) -> NetStatus;    // PeerId, 수신 주소, 릴레이 경유 주소, NAT 상태, 연결 수, Tor 부트스트랩 진행률,
                                            // 등록한 부트스트랩의 연결 여부, 지금 쓰는 릴레이(고른 순서)
                                            // 릴레이 경유 주소는 릴레이마다 번갈아(앞 8개에 두 릴레이가 다 들게), 같은 릴레이 안에서는 IPv4·DNS 먼저
    fn last_status(&self) -> NetStatus;     // 마지막으로 알린 상태(기다리지 않음)
    async fn peer_listen_addrs(&self, peer: PeerId) -> Vec<Multiaddr>; // 연결된 피어가 identify로 알린 수신 주소
    fn add_bootstrap(&self, addrs: Vec<Multiaddr>);    // 데몬은 시험 도우미(`testkit::net`)에서만 부름(사용자 설정 없음)
    fn remove_bootstrap(&self, addrs: Vec<Multiaddr>); // DHT·AutoNAT·릴레이 후보에서도 뺌(다시 더할 때까지 받지 않음)
    fn tor_share(&self) -> Option<TorShare>;           // 이 스웜의 Arti를 다른 스웜(`NetConfig.tor_share`)에 넘김. Arti가 없으면 None
    fn add_addresses(&self, peer: PeerId, addrs: Vec<Multiaddr>);
    async fn dial_addr(&self, addr: Multiaddr) -> Result<PeerId, NetError>; // PeerId 없이 주소(연락처 링크의 onion)로 걸고 답한 노드
    async fn device_onion_host(&self) -> Option<String>; // 이 기기의 onion 주소(망 없이 키에서, 없으면 키를 만듦). Arti가 없으면 None
    fn forget_onion_keys(&self);                       // onion 서비스 키·상태를 지움(폐기된 기기의 데이터 지우기)
    async fn restart(&self, cfg: NetConfig) -> Result<(), NetError>; // 스웜을 새 설정으로 바꿔 띄움(이벤트 수신기는 그대로, 옛 스웜이 제때 넘기지 않으면 `Timeout`, 옛 스웜은 계속 돎). 데몬은 쓰지 않음(시험용)
    async fn peers(&self) -> Vec<ConnectedPeer>;       // 연결된 피어(`net.peers`)
    fn shutdown(&self); fn is_closed(&self) -> bool;
    async fn inspect(&self) -> Option<NetInspection>; // 지금 스웜의 실제 구성(조립한 전송 `TRANSPORT_*`·켜진 동작·DHT 역할·수신·알리는 주소). 누출 시험용
    fn tor_activity(&self) -> TorActivity;            // Arti 다이얼·스트림 시도 수(누출 시험의 동기화용)
    async fn tor_connect(&self, host: &str, port: u16, isolation: TorIsolation) -> Result<TorStream, NetError>;
        // libp2p 밖의 Arti TCP 스트림(Tor로 도는 기기의 링크 미리보기). 이름은 출구가 풂, 같은 `TorIsolation`끼리만 회로 공유
}
pub enum NetEvent { InboundRequest { peer: PeerId, req: Incoming, reply: oneshot::Sender<YpResponse> },
                    Gossip { from: PeerId, msg_id: MessageId, msg: GossipMessage },
                    Media { peer: PeerId, frames: Vec<Vec<u8>> }, // Tor 통화 소리 패킷(데몬이 보낸 피어·통화를 검사)
                    PeerConnected(PeerId), PeerDisconnected(PeerId), Status(NetStatus) }
impl Incoming { fn is_packed(&self) -> bool; fn open(self) -> Result<(YpRequest, Option<Opened>), Box<YpResponse>> }
    // 압축된 요청은 코덱이 풀지 않음. 데몬이 보낸 노드를 인증했을 때만 open()으로 풀고, Opened는 답을 만들 때까지 쥠
```

Tor 스웜(`NetMode::Tor`, 기능 `tor`, 기본 포함)은 Arti로 직접 만든 libp2p 전송을 씁니다. 나가는 연결은 모두 Arti 경유(onion과
TCP 평문 주소만, QUIC·릴레이 회로 없음), 들어오는 연결은 onion 서비스로 받고, identify·DHT IP 등록(Kademlia 클라이언트)·mDNS·
AutoNAT·DCUtR·릴레이·UPnP를 모두 끕니다([protocol.md](protocol.md#tor-전용-연결)). 앱 사용자 기기의 주 스웜은 늘 이것입니다.
`onion_listen = false`에 `tor_share`로 주 스웜의 Arti를 함께 쓰는 Tor 스웜은 신뢰 스위치의 출구 스웜입니다(받지 않고 Tor 출구로만 겁니다).
여기에 `tor_isolated`를 켠 것은 파일 스웜입니다(회로 하나에 스웜 하나, 큰 첨부의 조각을 상대 onion 주소로 회로 여럿에 나눠 받음, 데몬 `filelink.rs`).
직접 연결 스웜(`NetMode::Normal`)은 QUIC·TCP·릴레이·DCUtR·AutoNAT(설정하면 UPnP)를 쓰고 Arti를 조립하지 않아 onion 주소에 걸지
못합니다. IPFS 공개망 노드는 쓰지 않습니다. 데몬은 이것을 직접 연결 시더·시험의 주 스웜(`--direct`)과 P2P 쪽 직접 스웜(P2P 전용
채널·신뢰 스위치)에만 씁니다. identify는 onion 연결에서 쓰지 않고 onion·빈
외부 주소 후보를 버립니다(`OnionQuiet`, AutoNAT·DCUtR로 새지 않게).

Tor 스웜의 전송은 `TRANSPORT_ARTI_ALL`·`TRANSPORT_ONION_LISTEN`뿐이어야 하며, `tests/tor_leak.rs`(이 크레이트와 데몬)가
`inspect()`와 127.0.0.1 미끼 수신기로 이를 확인합니다.

### yangpa-sfu

통화 SFU 엔진(sans-IO). str0m 0.24(`aws-lc-rs` 암호, 시스템 OpenSSL 불필요)로 참가자마다 `Rtc` 하나를 두고, 들어온 RTP
프레임을 풀지 않고 같은 통화방의 다른 참가자에게 넘깁니다. 소켓·시계·신호 경로는 데몬(`sfu.rs`)이 맡습니다.

- `Sfu::new(SfuConfig)`, `add_local_candidate`, `set_allowed(now, room, devices)`(들어올 수 있는 기기, 빠진 기기의 세션은
  닫음), `close_room`, `handle_signal(now, room, from, SignalPayload)`(`SfuOffer`·`SfuAnswer`·`SfuIce`·`SfuSelect`·
  `SfuClose`), `handle_receive`(UDP 데이터그램), `handle_timeout`, `poll_output() → Output{Transmit | Signal | Event}`,
  `poll_timeout`, 점검용 `sessions`·`forwarding_layer`.
- 협상: 참가자가 세션마다 한 번 제안하고 재협상은 엔진만 제안(글레어 없음). simulcast 층 고르기(`layer::choose`)는 받는
  쪽 선택이나 대역폭 추정으로, 층을 바꾸면 키프레임을 청하고 타임스탬프를 이어 붙임.
- `stun`: 서버 반사 주소를 알아내는 작은 STUN 바인딩 클라이언트(서버 목록은 데몬의 `net.stun_servers`). `testkit`: str0m 참가자(`TestPeer`)로 같은 프로세스 안에서
  엔진을 시험하는 도우미(데몬 시험도 씀).

### yangpa-rpc

JSON-RPC 2.0 타입, 길이 접두 프레이밍(JSON 프레임 + 바이너리 프레임), 클라이언트·서버 유틸리티, 그리고 모든 API 메서드의
요청·응답 타입(`api` 모듈). CLI·MCP·데스크톱 앱(Tauri 쪽)이 같은 타입을 씁니다. 세부는 [api.md](api.md).

### yangpa-daemon (`yangpad`)

```
src/
  main.rs        — 인자(데이터 경로, 엔드포인트, 수신·부트스트랩 주소, 키 보관 방식, 로그 수준, 받은 첨부 디스크 한도
                   `--cache-limit`·`--cache-min-free`, 보관 노드 `--archive`·`--archive-quota`, 휘발성 첨부 한도
                   `--volatile-attachment-mib`·`--volatile-ram-mib`, SFU `--sfu-host`·`--sfu-port`·`--sfu-bind`·`--sfu-announce`·
                   `--sfu-capacity`, 연결 방식 `--direct`·`--relay-server`), 노드 + RPC 서버, 종료 신호, 윈도에서 전원 조절(EcoQoS) 빠지기
  config.rs      — NodeConfig(데이터 경로, DB 키 보관 방식, 네트워크, 동기화 간격, 커밋 묶음 창, 공유·저장 공간 한도
                   `SharingOptions`, 휘발성 `VolatileOptions`, 보관 노드 `ArchiveOptions`, SFU `SfuOptions`. 네트워크
                   `NetOptions`의 `direct`(직접 연결 주 스웜)·`p2p_listen`(P2P 스웜 수신 주소, 명령줄 인자 없음, 시험은 루프백))
  node.rs        — Node: Store, NetHandle, 키 그룹 상태, 공간 런타임을 묶는 중심. 테스트에서 여러 개 띄울 수 있음
  identity.rs    — DB 키(OS 키 저장소 또는 0600 파일), 신원·기기 생성, 서명된 기기 목록·프로필, 승인한 새 기기 더하기(`add_device`),
                   연결한 기기의 첫 동기화(`spawn_first_sync`), 인증 거절(Revoked), 폐기된 기기 지우기
  devlink.rs     — 새 기기 연결: 링크·QR 띄우기(`device.link.*`), 인증 전 요청(`DeviceLink`·`DeviceLinkPoll`), 6자리 번호, 승인한 묶음 봉인·설치
  qr.rs          — 화면에 띄울 QR 코드(기기 연결 링크, 브리지 공유)
  supporter/     — 후원 모듈(별도 라이선스, LICENSE-SUPPORTER.md). mod.rs·api.rs(`supporter.*`·`flair.*` RPC)·flair.rs
  supporter/mod.rs — 후원자 확인(앱의 라이브러리 탭·콘텐츠 뷰어): GitHub 기기 로그인·Patreon PKCE 로그인(둘 다 비밀키 없음, Patreon은
                   `127.0.0.1:47900`으로 돌아옴), 정기·1회 후원 판정, 하루 한 번 다시 확인, 기존 사용자 유예, 결과를 내 다른
                   기기에 나누기(`SupporterShare`, 인증 직후 `on_authenticated`·`on_auth_accepted`). 요청은 링크 미리보기의
                   HTTP(`blobs::preview::http_request`; GitHub는 Tor로 도는 기기면 Arti, Patreon은 Tor 출구 차단으로 직접 연결), 토큰은 DB 설정에만
  badges.rs      — 도전과제 뱃지: 기준(친구·소유한 크루 멤버 수·시딩·통화·화면 공유 시간·후원액) 보기와 내 카드 서명(1분마다, 얻은 뱃지는 남김),
                   내 기기끼리 기기별 값 나누기, 카드 퍼뜨리기(`Badges`·`MemberBadges`), 받은 카드(`badges.card.<사용자>`),
                   개발자 계정의 원년멤버 증서(`badges.granted.<사용자>`), 시딩 바이트(`blobs::serve`가 더함),
                   통화·화면 공유 시간(1분마다 `voice::talking`을 봄)
  cards.rs       — 프로필·뱃지 카드를 빠뜨리지 않기: 공간마다 실린 판 기억하고 빠진 공간에 내기(`cards.sent.<공간>`, `MemberProfile`·`MemberBadges`),
                   체크포인트로 들어온 뒤 묻힌 프로필·카드 물어 받기(`GetMemberCards`, `cards.pull.<공간>`)
  supporter/flair.rs — 후원자 표시(카드 배너 월 14달러·후광 월 50달러 이상, GitHub 1회 후원은 30일): 확인한 단계와 후광 색을 상태 이벤트(`MemberFlair`)로 내
                   공간마다 내기, 해소기가 적용한 것을 사용자마다 모으기(`flair.user.<사용자>`, `crews::on_resolved`),
                   내 표시가 빠지거나 경계 아래로 묻힌 공간에 다시 내기(`flair.sent.<공간>`, 상태가 바뀔 때·`supporter::check_due`)
  presence.rs    — 표시 상태(온라인·자리 비움·숨기·오프라인, 기간): 내 상태를 인증된 기기에 보내기(`Presence`), 받은 기기
                   상태로 화면 상태 정하기(`shown_all`), 마지막 동기화 시각(`presence.seen.<사용자>`)과 비공개, 오프라인
                   모드(망 없이 다시 뜨기, `Node::restart_requested` → main.rs가 같은 인자로 새 데몬을 띄움)
  peers.rs       — 피어 인증(Auth), PeerId ↔ (사용자, 기기), P2P 스웜 인증 따로(`p2p_authed`), 주소 기록과 재연결
  friends.rs     — 연락처 링크 생성·폐기·사용, 사람 수락 없이 임시 연락처·1:1 공간, 만료, 친구 추가(`propose`), 친구 삭제
  author.rs      — 이벤트 작성(부모·논리 시계·키 고르기·암호화·서명)
  ingest.rs      — 받은 이벤트 적용(검증·중복·보류·레인별 처리·키 대기·키 그룹 정리·보기 행·알림). 적용 잠금은 공간마다
                   (`lock_space`: 관문 읽기 + 그 공간 잠금), 여러 공간을 다루는 쪽만 관문 쓰기(`lock`), 잠금 순서는 `Locked` 주석.
                   상태가 바뀌면 마지막으로 쓴 판(`SpaceCore::persisted`)에 비춰 바뀐 행만 씀
  keys.rs        — 상태 키 행, 채널 뿌리 저장소(뿌리 ID → 뿌리, 읽기용 캐시), 키 그룹 멤버 상태·확정 기기 표의 잠긴 저장,
                   가입자 상태 키(체크포인트로 가입하면 그 체크포인트에서 유효한 키와 그 뒤 키 가운데 최근 64개, 없으면 최근 64개. 나머지는
                   가입자가 `GetStateKeys`로 물음)와 키 묶음 나누기
  spaces.rs      — 공간 런타임, 시작 때 다시 세우기(바탕 체크포인트가 있으면 스냅숏 + 경계 위만), 공간 만들기(첫 크루 키 갱신까지
                   한 번에)
  checkpoint.rs  — 상태 체크포인트: 내기(소유자·위임 기기, 문턱·가라앉음·합치기), 받은 뒤 스냅숏 블롭 다시 만들어 시드(최근 2개)·
                   이견, 재시작(restore), 가입(prepare_join → try_install → confirm(두 번째 출처, 갈리거나 확인 못 하면 fall_back), 확인 전엔
                   쓰기 막음), 경계(floor·below, 경계 아래 부모를 해소기에 순서 연결로), 겨룬 체크포인트 경보(alert),
                   `GetCheckpoint`·`GetDeviceKeys` 답하기와 받은 기기 키 거르기(`prefetch_device_keys`: 확정 기기 봉인 전에도),
                   내 늦은 이벤트 다시 서명(resign)
  keygroup.rs    — 키 그룹(protocol.md "키 그룹"·"데몬 연결"): 묶음 창(300ms, 제거가 이어지면 2초, 기다리는 보내는 쪽만 바로),
                   지금 크루 상태 키, 보내는 쪽 규칙(제거를 반영한 회전을 기다림), 확정 기기 표 넘기기(`KeyTableHandover`)와
                   크루 밖 확정 기기에게 상태 키 인계. 하위: `committer`(표를 들고 원하는 그룹에 맞춰 `KeyUpdate`를 냄, 가입
                   묶음·조각 블롭, 그룹마다 두 칸에 저장), `member`(받아들인 갱신을 적용해 그룹 키·상태 키를 얻음, 그룹 사슬
                   (체크포인트 키 머리에서 시작할 수 있음), 모르는 뿌리 예산), `relay`(`GetChannelRoots`·`GetGroupKeys`·
                   `GetStateKeys`), `shards`(전체 재키의 사용자별 조각 블롭, `GetKeyShard`, 내주기 예산)
  messages.rs    — 메시지 보기 변환(익명 글 가리기·가명), 알림 문구, 보낼 때 쓸 기록 키
  anon.rs        — 익명 채널의 가명((공간, 채널, 사용자)에서 정하는 '형용사 + 동물')
  typing.rs      — 글 쓰는 중(`channel.typing`, 노드 간 `Typing`): 보내는·받는 간격, 받을 기기, 받은 것 확인(RAM만)
  paste.rs       — 붙여넣은 첨부(`blob.paste`): 경로 없는 첨부 바이트를 0으로 덮는 RAM 버퍼에 두고 보낼 때 빌려 줌
  sync_service.rs— 실시간 전파(직접/gossip, `NoPeers`·볼 수 있는 기기가 256대 넘는 채널은 연결된 곳 먼저 8곳까지),
                   따라잡기(머리·부모·지문 비교, 빈 부모 채우기는 작성 기기별 제한을 탐), 시더 응답(5단계 검사).
                   gossip 허용 목록(`member_peers`)은 아는 노드(인증된 연결·기기가 묶인 노드 기록)를 한 번 훑어 멤버 기기의
                   것만 고름(활성 기기마다 DB를 찾지 않음)
  state_index.rs — 상태 색인: 활성 기기 수·목록, 기기 → 사용자 표, 채널을 누가 볼 수 있는지(권한 규칙이 같은 채널끼리 한 번)를
                   상태 사본마다 한 번 계산(사본을 강한 참조로 들고 포인터로 맞춤, 기기 → 사용자 표는 앞 사본에서 바뀐 멤버만 고침).
                   전파·받기의 멤버 기기 확인·확정 기기의 그룹 맞추기가 씀
  devkeys.rs     — 기기 키 표: 상태 기록의 키 약속으로 상자 키를 찾음(DB `device_keys` 앞의 LRU 4,096개). 받은 상태 이벤트
                   본문에 실린 목록의 키(`body_keys`)를 이벤트와 한 트랜잭션으로 그 공간 몫 행에 넣음(서명 확인 없음: 내용
                   주소). 봉인하는 곳(확정 기기·인계·신고·휘발성·통화 키)은 모두 이것
  ratelimit.rs   — 범위별 올리기·받기 속도 제한, 큰 크루에서 곧바로 받은 상태·키 갱신 이벤트의 작성 기기별 토큰 버킷
  inbound.rs     — 노드 간 요청을 영역별 처리기로 나눔
  notify.rs      — 알림(로컬 API 구독자·프로세스 안 구독자)
  crews.rs       — (B1) 크루·채널·역할·초대·이모지·비공개 채널
  volatile.rs    — (B2) 휘발성 채널: RAM 저장, 수명 타이머, 입장·퇴장, 세션 키 갱신, 늦은 입장자 스냅숏
  volatile/attach.rs — (X1) 휘발성 채널 첨부: RAM에만 올리고 참가자끼리 받고 내줌, 메시지와 함께 지움
  diag.rs        — (B2) 지표(전달 지연·처리량·메모리·CPU·대기열), 휘발성 누출 검사
  blobs.rs       — (B3) 첨부 업로드·스웜 다운로드·제공, 프로필·이모지·크루 아이콘 이미지, 링크 미리보기
                   (blobs/: prepare·fetch·serve·preview, (X4) cache.rs 받은 첨부 저장 공간 정리·고정(디스크 블롭만,
                   휘발성 RAM 저장소는 건드리지 않음), prefetch.rs 헤드리스 미리 받기, media.rs·hints.rs 동영상·음성 힌트)
  policy.rs      — (B3) 시더·피어 설정, 열어 본 채널, 휴면 공간
  voice.rs       — (B5) 통화 참가자 목록·참가 자격, 시그널링 중계, 프레임 키 배포, (X2) 메시/SFU 결정·호스트 선출·넘겨주기
  sfu.rs         — (X2) SFU 호스트 실행기: yangpa-sfu 엔진을 UDP 소켓·시계·통화 신호 경로에 이음, 맡을 수 있는지(Tor)
  admin.rs       — (B4) API 토큰, 감사 로그, 사람 확인 대기열
  notes.rs       — (B4) 친구 메모(내 기기끼리만)
  tor.rs, locator.rs — (B5) 시작할 네트워크 설정(Tor 또는 `--direct`, 예전 모드·없어진 망 설정 지우기), 위치 기록(DHT)
  stun.rs        — (B5) STUN 서버 목록(`net.stun_servers`): 기본 목록·검사·`stun_servers(node)`(직접 연결 경로만 씀),
                   크루 STUN 서버(`crew.set_servers`·`crew.use_servers`, `space.get`의 칸, 직접 스웜 묻기 `crew_stun_all`,
                   통화 ICE 서버 `call_ice`)
  p2p.rs         — P2P 전용 채널과 신뢰 스위치의 연결: 기기별 참여 동의, 필요한 동안만 띄우는 직접·출구 스웜(새 임시 키),
                   `P2pHello`·P2P 인증, STUN·UPnP·Tor로 맞추는 홀펀칭, 길 나누기, 연결 방식(`ConnectionMode`)
  filelink.rs    — Tor 파일 회로: 큰 첨부를 Tor로 받을 때 회로마다 띄우는 파일 스웜(새 임시 키, 받지 않음)과
                   `FileLink` 인증, 들어온 파일 링크의 블롭 요청을 그 기기의 주 연결로 답하기
  bridges.rs     — Tor 브리지(설정 `net.bridges`): lyrebird 찾기, 내장·직접 넣은 브리지를 Arti에 주기, 막혔을 때 자동 Snowflake
  trust.rs       — 신뢰 스위치: 이 기기의 IP를 보여도 되는 사람·크루(설정 `trust.users`·`trust.spaces`), `trusts` 판정
  moderation.rs  — (W2) 밴의 최근 메시지 삭제, 차단(이 기기만), 신고 봉인 전달·받기·처리 결과 공유
  archive.rs     — (X3) 보관 전용 노드: archive/host.rs(보관 노드 쪽 지정 받기·명단·당겨 오기·내주기·첨부·해제),
                   archive/member.rs(멤버 쪽 지정·해제·명단 보내기·바탕 명단 위 차이·가입자 넣기·이벤트 밀어 보내기·첨부와
                   스냅숏 알림·삭제 반영·커버리지)
  testkit.rs     — 통합 시험 도우미(공개 API 아님)
  api/           — RPC 핸들러·토큰 인증·디스패치(mod.rs)와 영역별 메서드 파일(X4 `storage.rs`, X3 `archive.rs` 포함)
```

영역(B1~B5, W2~W4, X1~X4)별 파일 소유와 코어가 부르는 고리는 [daemon.md](daemon.md)의 "모듈 소유"를 보세요. 영역을 넘는
시나리오는 `tests/integration.rs`(휘발성 첨부·저장 공간 정리·보관 노드 사이의 경계 포함), Tor 누출은 `tests/tor_leak.rs`,
성능 목표·여러 노드 시뮬레이션은 `tests/perf.rs`([performance.md](performance.md)).

영역 사이의 약속:
- 휘발성 첨부(X1)는 `VolatileStore`(RAM)에만 있고, 저장 공간 정리(X4)는 디스크 블롭(`Store::blob_drop_data`)만 지웁니다.
  `blob.status`의 `pinned`·`kept`는 휘발성 첨부에서 늘 `false`이고 고정(`blob.pin`)은 `1004`입니다.
- 보관 범위(`yangpa_sync::archive::archived_channels`)는 휘발성 채널을 빼고, 멤버는 휘발성 키로 쓴 이벤트를 보관 노드에
  보내지 않으며(`archive/member.rs`), 보관 노드도 받지 않습니다(`archive/host.rs`, 일반 등급 저장소만 씀). 휘발성 첨부는
  참가자에게만 내주므로 보관 노드의 블롭 요청은 거절됩니다. 보관 노드가 받아 둔 첨부는 저장소 `pinned`라 저장 공간 정리 대상이 아니고
  `--archive-quota`로만 묶이며, 미리 받기는 멤버인 공간에서만 돕니다(받아 둔 공간은 `node.spaces`에 없음).
- SFU 호스트(X2)는 Tor로 도는 기기에서 맡지 않습니다(`sfu::can_host`). P2P 전용 채널의 통화도 같아, 직접 연결로 띄운
  시더가 아니면 메시로 남습니다.

### yangpa-bench

성능 기준 작업 묶음(암호·저장·지문 비교·적용)과 `yangpa-bench` 실행 파일(`list`·`run`·`measure`·`gate`·`time`).
CI(`.github/workflows/bench.yml`)는 작업마다 valgrind cachegrind로 한 번당 명령 수를 재 기준과 비교하고, 허용치를 넘게
늘면 실패합니다. 빌드는 `[profile.perf]`(release에서 LTO 끄고 codegen-units 16, 패닉 풀기). 세부는 [performance.md](performance.md).
규모 시뮬레이션(`yangpa-bench scale`)의 `--checkpoint`는 체크포인트로 새 멤버가 받는 양과 차가운 시작을 재고, 차가운 시작은 숨은
명령 `scale-checkpoint-load`(새 프로세스)에서 잽니다.

### yangpa-cli (`yangpa`)

기획서의 명령 구조(`yangpa crew create`, `yangpa channel create --volatile --join-leave --ttl 30s` …)로 API 전체를 노출합니다.
모더레이션은 `yangpa member ban|unban|bans`, `yangpa user block|unblock|blocked|devices|block-device|unblock-device|blocked-devices`, `yangpa message report`, `yangpa report list|resolve`
(`report`는 `reports`로도 부름). 새 기기 연결은 쓰던 기기의 `yangpa device link|link-pending|link-approve|link-reject|link-cancel`과
새 기기의 `yangpa device join|join-abort`(링크는 표준 입력)입니다.
보관 전용 노드는 `yangpa archive offer|hosted|drop`(보관 노드 쪽), `yangpa archive designate|revoke|list`(크루 쪽).
받은 첨부 저장 공간은 `yangpa blob cache status|limit|clear`, `yangpa blob pin|unpin|pins`, 헤드리스 미리 받기는
`yangpa seed set --prefetch on|off`, 확정 기기는 `yangpa crew committer`. 신뢰 스위치는 `yangpa trust get|user|crew`(`--off`로 끄기,
켤 때는 IP 노출 주의를 표준 오류로 알림), STUN 서버 목록은 `yangpa net stun`.
`yangpa daemon` 하위 명령은 헤드리스 모드로 데몬을 띄웁니다.

### yangpa-mcp (`yangpa-mcp`)

MCP stdio 서버. API를 도구로 노출합니다. 파괴적 작업(추방·밴·밴 해제·`dismiss`가 아닌 신고 처리·삭제·권한 변경·초대 생성)은
데몬의 사람 확인 대기열에 올리고 사람이 GUI/CLI에서 승인해야 실행됩니다. 휘발성 채널 읽기는 채널별 명시 허용 전까지 거부됩니다.
모더레이션 도구는 `ban_member`, `unban_member`, `list_bans`, `block_user`, `list_blocked`, `report_message`, `list_reports`,
`resolve_report`이고 차단 해제 도구는 일부러 두지 않습니다. 차단한 사람의 메시지는 도구 결과에서도 내용 없이 접습니다.
보관 전용 노드 도구는 `list_archives`, `designate_archive`, `revoke_archive`(뒤의 둘은 사람 확인), 저장 공간 도구는
`get_storage_usage`, `set_storage_limit`, `clear_storage_cache`, `pin_attachment`입니다.

### apps/desktop

Tauri 2 + React + TypeScript(Vite). 같은 코드로 Android 앱도 빌드합니다(데몬은 앱 프로세스 안, [android.md](android.md)).
Tauri 쪽(Rust)은 데몬을 사이드카로 띄우거나 이미 떠 있는 데몬에 붙고,
`rpc_call` 명령과 이벤트 전달, 블롭용 커스텀 프로토콜(`yangpa-blob://`, `Cache-Control: no-store`)을 제공합니다.
통화 음성의 신경망 코덱(Mimi·DAC)은 앱의 Rust 쪽 `src-tauri/src/neural/`이 ONNX Runtime(`ort`, load-dynamic)으로
계산하고(명령 `neural_status`·`neural_encode`·`neural_decode`·`neural_close`, 소리·코드는 바이트 본문),
웹뷰의 `voice/neural.ts`가 캡처·재생 워클릿(`voice/neural-worklets.ts`, 24kHz)과 상대마다 데이터 채널로 나릅니다. 모델과
ONNX Runtime은 실행 파일·설치 파일 밖의 모델 폴더(`YANGPA_MODELS_DIR` → 실행 파일 옆 `models` → `%APPDATA%\yangpa\models`)에
두고 배포는 따로 묶습니다(`scripts/neural-models.txt`, protocol.md "신경망 음성 코덱"). 앱은 열기 전에 런타임·모델의 sha256을
같은 목록과 맞춰 보고, 다른 파일은 빠진 파일처럼 다룹니다(그 코덱은 쓰지 않고 Opus).
화면공유의 ffmpeg 인코더(AOM AV1·SVT-AV1·NVENC AV1/H.264/HEVC)는 앱의 Rust 쪽 `src-tauri/src/screen/`이 실행 파일 옆
`ffmpeg` 폴더(`YANGPA_FFMPEG_DIR` → 실행 파일 옆 → 데이터 경로 옆)의 `ffmpeg.exe`(필요한 것만 넣어 직접 빌드한 LGPL 2.1+ 판, 저장소 `vendor/ffmpeg`, `scripts/build-ffmpeg.sh`)를 따로 띄워(잡 객체로 앱과 함께
끝남, 띄우기 전 sha256 확인, `scripts/ffmpeg-files.txt`. 목록에 없는 DLL·실행 파일이 폴더에 있으면 띄우지 않고, 확인한
파일은 쓰기·지우기를 막고 연 채 ffmpeg가 끝날 때까지 쥠. 웹뷰를 새로 고치면 캡처를 멈춤) `gfxcapture`로 고른 창·모니터를 캡처·인코딩하고, 표준 출력의 FLV에서
설정 레코드·프레임을 꺼내 채널로 웹뷰에 넘깁니다(명령 `screen_status`·`screen_sources`·`screen_start`·`screen_stop`).
화면 소리는 `src-tauri/src/screen/audio.rs`(명령 `screen_audio_start`·`screen_audio_stop`)가 WASAPI 프로세스 루프백(윈도 10 2004
이상)으로 잡습니다: 창을 공유하면 그 창의 프로세스 트리 소리만, 그 밖에는 이 앱 프로세스 트리(웹뷰가 내는 통화 소리 포함)를 뺀
시스템 소리. 48kHz 스테레오 f32 PCM을 10ms 남짓씩 채널로 넘기고, 웹뷰의 `voice/screen-audio.ts`가 오디오 트랙으로 만듭니다
(인코더 종류와 상관없이 같음, 메시 통화만).
웹뷰의 `voice/native-screen.ts`가 프레임을 봉인해 상대마다 데이터 채널로 나르고, 받은 프레임은 WebCodecs로 풉니다
(protocol.md "화면공유 ffmpeg 인코더"). 링크하지 않고 따로 띄우는 까닭은 ffmpeg가 죽어도 키를 가진 앱 프로세스가 남고 LGPL
조건이 단순하기 때문입니다. 모델 폴더와 같은 sha256 확인은 `src-tauri/src/pinned.rs`가 함께 씁니다.
웹 초대 페이지(`web/index.html`)의 '앱에서 열기'가 띄운 `yangpa://` 링크는 `src-tauri/src/deeplink.rs`가 하나만 받아 두고
(`yangpa-open-link` 이벤트, 명령 `deep_link_take`) 웹뷰가 친구 추가·크루 참가 창, 신원이 없으면 온보딩의 기기 연결 칸에 채웁니다(`lib/links.ts`). 앱은 single-instance로
하나만 뜨고, 두 번째 실행은 링크만 넘깁니다.
기기 연결 링크의 복사는 `src-tauri/src/secret.rs`(명령 `secret_copy`)가 맡습니다. 윈도 클립보드에는
기록·클라우드 동기화에서 빼는 표시를 붙여 넣고 1분 뒤 그대로면 비웁니다.
데스크톱 앱 데이터 폴더: 윈도 설치판·포터블(실행 파일 옆에 `yangpa-desktop.exe`가 있음)이면 `yangpa-rpc`의
`endpoint::data_dir`가 `desktop_data_dir_in`으로 데이터 경로를 정합니다. 설정 파일 `data-dir.json`의 `data_dir`(상대 경로는
실행 파일 폴더 기준)가 먼저이고, 없으면 실행 파일 옆 `Data`입니다. 다만 `Data`에 양파 데이터가 없고 OS 기본 경로
(`%APPDATA%\yangpa\data`)에 있으면 예전 판 데이터로 보고 옮기지 않고 그 경로를 씁니다. 앱 폴더에 쓸 수 없으면(Program Files에
깐 MSI) 기본은 OS 기본 경로이고 설정 파일은 그 옆(`%APPDATA%\yangpa\data-dir.json`)에 둡니다. 설정 파일은 설치 파일·zip에
넣지 않아 업데이트로 덮이지 않고, NSIS 제거 프로그램은 설치 폴더를 통째로 지우지 않아(`RMDir "$INSTDIR"`) `Data`가 남습니다.
엔드포인트는 그 폴더에 딸린 것(`Endpoint::in_dir`, 윈도는 경로를 소문자로 맞춘 이름의 named pipe)이라 같은 PC의 다른 설치판·
포터블 데몬과 겹치지 않고, OS 기본 경로를 쓰면 예전처럼 사용자 이름의 파이프입니다. 앱·데몬·CLI가 같은 폴더에 있어 셋 다 같은
판단을 합니다. 앱은 UI 설정·알림 프사 캐시를 `<데이터>/app`(처음엔 예전 OS 앱 설정 경로의 UI 설정을 한 번 가져옴), 웹뷰
저장소를 `<데이터>/webview`에 둡니다. 폴더 바꾸기는 `src-tauri/src/datadir.rs`(명령 `data_dir_info`·`data_dir_plan`·
`data_dir_change`·`app_restart`, 진행 이벤트 `yangpa-data-move`): 설정 파일을 쓸 수 있는지 먼저 확인하고, 데몬을 끄고 데이터 경로
잠금(`yangpad.lock`)을 쥔 채 맨 위 항목마다 같은 드라이브면 이름 바꾸기, 아니면 복사(크기 확인) 뒤 원본 삭제, 실패하면 되돌림,
`data-dir.json`을 쓰고 앱을 다시 시작합니다. DB 키의 OS 키 저장소 계정 이름은 처음 쓴 데이터 경로의 해시라, 데몬이 데이터
폴더의 `db.keyring`에 적어 두고 다음부터 그 이름을 씁니다(폴더를 옮겨도 같은 키).
음성·화면공유는 웹뷰 WebRTC + 프레임 암호화(보내는·받는 트랙마다 변환, `createEncodedStreams`가 있으면 그것, 없으면
`RTCRtpScriptTransform`. 변환은 트랜시버를 만들자마자 달고 트랙은 그 뒤에 넣음, decisions.md "통화 프레임 암호화 방식"). `voice/session.ts`가
메시(참가자 쌍마다 연결)와 SFU(호스트에 연결 하나, 화면 simulcast 3층, 받을 층 고르기)를 `voice.participants`의 토폴로지에
따라 오갑니다.
앱의 API 타입(`src/api/types.ts`)은 `yangpa-rpc`의 `desktop_contract` 시험이 메서드 표에서 만드는
`src/api/methods.generated.json`과 vitest로 맞춰 봅니다(어긋나면 양쪽 시험이 실패).

- 사이드카 계약: `tauri.conf.json`의 `bundle.externalBin`은 `binaries/yangpad`, 릴리스 파이프라인(`release.yml`)이 같은 타깃으로
  빌드한 데몬을 `src-tauri/binaries/yangpad-<타깃 트리플>[.exe]`(git 무시)에 둔 뒤 `tauri build`. 그 파일이 없는 개발 빌드는
  `build.rs`가 `externalBin`을 빼고 빌드하고, 앱(`src-tauri/src/daemon.rs`)은 `YANGPA_DAEMON_BIN` → 실행 파일 옆 → 이름을 바꾸기
  전 사이드카(`yangpad-<트리플>`) → (디버그 빌드만) 작업 공간 `target` → `PATH` 순으로 `yangpad`를 찾습니다. Windows NSIS 설치·제거는 훅(`windows/installer-hooks.nsh`)이 설치 경로의
  `yangpad.exe`를 먼저 끕니다.
- 화면: 모더레이션(신고·신고 대기열·밴 목록·차단한 사용자, 차단한 사람의 메시지 접기), 새 기기 연결(온보딩의 연결 화면, 설정 › 기기의 QR·링크, 앱 어디서든 뜨는 허용 창: `modals/DeviceLink.tsx`), 휘발성 채널의 API 읽기 허용, 네트워크·Tor 탭의 STUN 서버 목록, 휘발성 채널 첨부
  (메모리에만 보관, 저장은 경고 확인 뒤), 저장 공간 한도·정리·고정, 동영상·음성 썸네일·길이, 크루 설정의 확정 기기와
  보관 전용 노드, 통화의 SFU 모드.
- 시험: vitest 단위·컴포넌트(happy-dom)와 실제 `yangpad` 2~3개를 띄워 앱의 API 클라이언트로 부르는 종단 간 시험(`src/e2e`,
  `YANGPA_E2E_REQUIRED=1`이면 데몬이 없을 때 실패), 리눅스에서 실제 창을 띄우는 시험(`npm run test:window`, tauri-driver·
  WebKitWebDriver).

## 데이터 흐름 예: 메시지 보내기(일반 등급)

1. 클라이언트 `message.send` → 데몬이 권한 검사(`sync::state::authorize_message`).
2. 보내는 쪽 규칙(`keygroup::wait_rotation`): 이 채널의 키 그룹에 반영되지 않은 제거가 있으면 회전을 기다림(기본 30초,
   `strict_rotation`이면 끝없이, 넘기면 옛 키로 보내고 `key_rotation_pending`). 그 뒤 채널이 쓰는 그룹의 정규 사슬 끝에서
   뿌리를 파생(기록 공개면 `public_root(R)`, 아니면 `private_root(T)`). 따로 내는 키 묶음은 없습니다. 본문 키는
   `keysched::record_key(뿌리)`, 헤더는 `KeyRef::Record{root}`.
3. `EventBody::Message` → `crypto::encrypt_body` → `SignedEvent::sign`.
4. 저장: 이벤트(본문은 메시지별 키로 다시 봉인) + 메시지 보기 행, 한 트랜잭션.
5. 전파: 기기 8대 이하 공간은 연결된 멤버 노드에 `PushEvents`, 그보다 크면 gossip 토픽(작성자는 메시 피어에게만 올림).
   토픽에 받을 피어가 없으면 연결된 멤버 기기 먼저 8곳까지만 직접 보내고 나머지는 따라잡기로 받습니다.
6. 받는 쪽: 서명·기기 목록·권한 검사 → 복호화 → 저장 → UI에 프레임 단위로 묶어 알림.
