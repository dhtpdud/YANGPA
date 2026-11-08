//! 서명된 이벤트와 이벤트 DAG의 기본 단위.
//!
//! - 이벤트 ID = BLAKE3(postcard(헤더) || 서명). 본문은 ID에 들어가지 않고 `body_hash`로만
//!   묶이므로, 삭제·수명 만료 시 본문만 버려도 DAG가 깨지지 않습니다.
//! - 본문은 항상 암호문입니다(상태 키·기록 키·휘발성 세션 키, 또는 받는 기기마다 봉인). 평문 본문은 없습니다.
//! - 부분 전달 스트림(`scope`가 있는 이벤트)은 일부 기기에게만 갑니다. 공간·채널 스트림은 이들을 부모로
//!   가리키지 않으므로, 그것을 못 받은 노드도 같은 DAG·같은 상태를 얻습니다.

use serde::{Deserialize, Serialize};

use crate::device::DeviceSignKeys;
use crate::error::CoreError;
use crate::ids::{
    ChannelId, ChannelRootId, DeviceId, EventId, KeyGroupId, SpaceId, StateKeyId, UserId,
};
use crate::sign::{Sig, SignDomain, SigningKeypair, verify};
use crate::time::Timestamp;

/// 이벤트 형식 판. 2판: 기기 목록 항목에 서명 공개키(`DeviceEntry.sign_keys`)가 실리고, 헤더 끝에 `scope`·
/// `state_anchor`가 붙습니다(헤더 필드 14개). 3판: 그룹 키 합의가 키 그룹(`KeyUpdate`)으로 바뀌며 [`KeyRef`]에서
/// 에포크·기록 키·평문 변형이 빠지고 채널 뿌리(`Record`)가 들어와 변형 번호가 바뀌었습니다. 앞 판 이벤트는 헤더의 키 표시부터 뜻이 달라 머리에서 바로 거릅니다
/// (본문 복호화·디코딩 실패를 상대 잘못으로 셈하지 않게).
pub const EVENT_VERSION: u8 = 3;
/// 부분 전달 이벤트가 가리키는 공간 스트림 머리 최대 개수.
pub const MAX_STATE_ANCHOR: usize = 4;
/// 부모 해시 최대 개수.
pub const MAX_PARENTS: usize = 16;
/// 이벤트 본문 최대 크기. 큰 데이터는 블롭(파일 스웜)으로 보냅니다.
pub const MAX_BODY_LEN: usize = 256 * 1024;
/// 키 합의 레인(`KeyUpdate` 조각) 본문 최대 크기. 직접 인코딩 회전은 남은 기기마다 X-Wing 봉투(약 1.2KiB)를 실어
/// 일반 한도로는 256대 그룹이 막힙니다. gossip 메시지 한도(1MiB) 안에 이벤트 하나가 머리글과 함께 들어가게 잡았습니다.
pub const MAX_KEY_UPDATE_BODY_LEN: usize = 960 * 1024;

/// 이벤트 종류. 네트워크에는 u16 코드로 실립니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EventKind {
    // 메시지 레인: 채널 필수, 시더·피어 설정 적용.
    Message,
    Edit,
    Delete,
    Reaction,
    ReactionRemove,
    ForumPost,
    /// 익명 채널에서 쓴 `Message`. 본문은 `Message`와 같고, 서명된 헤더 종류가 익명으로 썼다는 표시입니다.
    AnonMessage,
    /// 익명 채널에서 쓴 `ForumPost`.
    AnonForumPost,
    // 키 레인: 그 채널을 볼 수 있는 멤버에게 필수.
    KeyShare,
    // 상태 레인: 모든 멤버에게 필수.
    SpaceCreate,
    SpaceUpdate,
    ChannelCreate,
    ChannelUpdate,
    ChannelDelete,
    RoleCreate,
    RoleUpdate,
    RoleDelete,
    MemberJoin,
    MemberUpdate,
    MemberKick,
    MemberLeave,
    InviteCreate,
    InviteRevoke,
    EmojiAdd,
    EmojiRemove,
    CrewConvertRequest,
    CrewConvertAccept,
    OwnershipTransfer,
    CommitterChange,
    DeviceListUpdate,
    ForumPostUpdate,
    MemberProfile,
    MemberBan,
    MemberUnban,
    /// 보관 전용 노드 지정·해제(`archive`).
    ArchiveGrant,
    ArchiveRevoke,
    /// 채널 내용 전송 방식 지정(`ChannelTransport`).
    ChannelTransportSet,
    /// 역할 표시 설정(`RoleDisplay`).
    RoleDisplaySet,
    /// 채널 익명 설정(`ChannelAnonymousSet`).
    ChannelAnonymousSet,
    /// 크루 STUN 서버(`CrewServersSet`).
    CrewServersSet,
    /// 포럼 대문 게시글(`ForumFeaturedSet`).
    ForumFeaturedSet,
    /// 채널 첨부 한도(`ChannelAttachLimitSet`).
    ChannelAttachLimitSet,
    /// 채널 수정·삭제 금지(`ChannelEditRulesSet`).
    ChannelEditRulesSet,
    /// 소유자가 함께 서명한 상태 체크포인트(`checkpoint::SignedCheckpoint`).
    StateCheckpoint,
    /// 표 예비 보유 기기 지정(`KeyBackupSet`, 소유자만).
    KeyBackupSet,
    /// 체크포인트 작성 위임(`checkpoint::DelegationCert`).
    CheckpointDelegation,
    /// 체크포인트 이견(정보용).
    CheckpointDispute,
    /// 휘발성 세션 키 회전 공지(`VolatileRotationBody`, 키 레인, RAM 전용).
    VolatileRotation,
    /// 상시 휘발성 채널 참가 기기 상한(`ChannelVolatileCapSet`).
    ChannelVolatileCapSet,
    /// 크루 커스텀 이모지 한도(`EmojiLimitsSet`).
    EmojiLimitsSet,
    /// 작성자 자신의 후원자 표시(`model::Flair`). 80~99는 키 합의 레인이라 100.
    MemberFlair,
    /// 작성자의 뱃지 카드(`MemberBadges`, 상태를 바꾸지 않음).
    MemberBadges,
    /// 크루 해산(`CrewDisband`, 소유자만).
    CrewDisband,
    // 키 합의 레인: 그룹 키 갱신. 본문은 상태 키로 암호화.
    /// 키 그룹 갱신 한 조각(`KeyUpdatePayload`). 크루 그룹은 공간 스트림, 비공개 그룹은 `KeyGroup` 스트림.
    KeyUpdate,
    /// 이 버전이 모르는 종류. 저장·전달은 하되 해석하지 않습니다.
    Other(u16),
}

/// 이벤트가 속한 레인. 동기화 필수 여부와 시더·피어 설정 적용 여부를 정합니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lane {
    Message,
    Key,
    State,
    /// 키 합의(`KeyUpdate`). 본문 한도가 큽니다([`MAX_KEY_UPDATE_BODY_LEN`]).
    KeyAgreement,
}

impl EventKind {
    const TABLE: &'static [(u16, EventKind)] = &[
        (1, EventKind::Message),
        (2, EventKind::Edit),
        (3, EventKind::Delete),
        (4, EventKind::Reaction),
        (5, EventKind::ReactionRemove),
        (6, EventKind::ForumPost),
        (7, EventKind::AnonMessage),
        (8, EventKind::AnonForumPost),
        (20, EventKind::KeyShare),
        (21, EventKind::VolatileRotation),
        (40, EventKind::SpaceCreate),
        (41, EventKind::SpaceUpdate),
        (42, EventKind::ChannelCreate),
        (43, EventKind::ChannelUpdate),
        (44, EventKind::ChannelDelete),
        (45, EventKind::RoleCreate),
        (46, EventKind::RoleUpdate),
        (47, EventKind::RoleDelete),
        (48, EventKind::MemberJoin),
        (49, EventKind::MemberUpdate),
        (50, EventKind::MemberKick),
        (51, EventKind::MemberLeave),
        (52, EventKind::InviteCreate),
        (53, EventKind::InviteRevoke),
        (54, EventKind::EmojiAdd),
        (55, EventKind::EmojiRemove),
        (56, EventKind::CrewConvertRequest),
        (57, EventKind::CrewConvertAccept),
        (58, EventKind::OwnershipTransfer),
        (59, EventKind::CommitterChange),
        (60, EventKind::DeviceListUpdate),
        (61, EventKind::ForumPostUpdate),
        (62, EventKind::MemberProfile),
        (63, EventKind::MemberBan),
        (64, EventKind::MemberUnban),
        (65, EventKind::ArchiveGrant),
        (66, EventKind::ArchiveRevoke),
        (67, EventKind::ChannelTransportSet),
        (68, EventKind::RoleDisplaySet),
        (69, EventKind::ChannelAnonymousSet),
        (70, EventKind::CrewServersSet),
        (71, EventKind::ForumFeaturedSet),
        (72, EventKind::ChannelAttachLimitSet),
        (73, EventKind::ChannelEditRulesSet),
        (74, EventKind::StateCheckpoint),
        (75, EventKind::KeyBackupSet),
        (76, EventKind::CheckpointDelegation),
        (77, EventKind::CheckpointDispute),
        (78, EventKind::ChannelVolatileCapSet),
        (79, EventKind::EmojiLimitsSet),
        // 80·81은 2판까지의 그룹 키 합의(MLS 커밋·웰컴) 코드라 다시 쓰지 않습니다(옛 이벤트가 다른 뜻으로 풀리지 않게).
        (82, EventKind::KeyUpdate),
        (100, EventKind::MemberFlair),
        (101, EventKind::MemberBadges),
        (102, EventKind::CrewDisband),
    ];

    pub fn code(self) -> u16 {
        if let EventKind::Other(c) = self {
            return c;
        }
        Self::TABLE
            .iter()
            .find(|(_, k)| *k == self)
            .map(|(c, _)| *c)
            .expect("모든 종류가 표에 있음")
    }

    pub fn from_code(code: u16) -> EventKind {
        Self::TABLE
            .iter()
            .find(|(c, _)| *c == code)
            .map(|(_, k)| *k)
            .unwrap_or(EventKind::Other(code))
    }

    pub fn lane(self) -> Lane {
        match self.code() {
            1..=19 => Lane::Message,
            20..=39 => Lane::Key,
            80..=99 => Lane::KeyAgreement,
            _ => Lane::State,
        }
    }

    /// 이 종류 본문의 최대 크기.
    pub fn max_body_len(self) -> usize {
        if self.lane() == Lane::KeyAgreement {
            MAX_KEY_UPDATE_BODY_LEN
        } else {
            MAX_BODY_LEN
        }
    }

    /// 본문 종류: 익명 글은 원래 종류(`AnonMessage` → `Message`), 나머지는 그대로입니다.
    pub fn base(self) -> EventKind {
        match self {
            EventKind::AnonMessage => EventKind::Message,
            EventKind::AnonForumPost => EventKind::ForumPost,
            k => k,
        }
    }

    /// 익명으로 쓴 글인지.
    pub fn is_anonymous(self) -> bool {
        matches!(self, EventKind::AnonMessage | EventKind::AnonForumPost)
    }

    /// 익명 채널에서 쓸 때의 종류. 메시지·게시글만 바뀌고 수정·반응 등은 그대로입니다.
    pub fn anonymized(self) -> EventKind {
        match self {
            EventKind::Message => EventKind::AnonMessage,
            EventKind::ForumPost => EventKind::AnonForumPost,
            k => k,
        }
    }

    /// 채널 ID가 반드시 있어야 하는 종류. 키 레인은 채널 키(채널 스트림)와 상태 키(공간 스트림) 둘 다 있습니다.
    pub fn requires_channel(self) -> bool {
        matches!(self.lane(), Lane::Message)
    }
}

impl Serialize for EventKind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u16(self.code())
    }
}

impl<'de> Deserialize<'de> for EventKind {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(EventKind::from_code(u16::deserialize(d)?))
    }
}

/// 본문을 어떤 키로 암호화했는지.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum KeyRef {
    /// 상태 키 S = state_key(R): 크루 그룹이면 공간 상태 키(상태 이벤트·크루 `KeyUpdate`), 비공개 그룹이면 그 그룹의
    /// 상태 키(`KeyGroup` 스트림의 키 묶음·`KeyUpdate`). ID는 키에서 나옵니다([`crate::body::state_key_id`]).
    State(StateKeyId),
    /// 채널 뿌리(공개 뿌리 = f(R), 비공개 뿌리 = f(T))에서 나온 기록 키. ID는 뿌리에서 나옵니다
    /// ([`crate::body::channel_root_id`]).
    Record { root: ChannelRootId },
    /// 휘발성 채널 세션 키(세대 번호).
    Volatile { generation: u64 },
    /// 지정 기기들에게 봉인(X-Wing)한 본문.
    Sealed,
}

/// 이벤트가 속한 스트림. 부모 해시는 같은 스트림의 이벤트만 가리킵니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum StreamId {
    /// 공간 수준(상태·키 합의 레인).
    Space,
    /// 채널(메시지·키 레인).
    Channel(ChannelId),
    /// 키 그룹(그 그룹 채널을 볼 수 있는 기기에게만 가는 키·키 합의 이벤트).
    KeyGroup(KeyGroupId),
    /// 한 사용자의 받은편지함(그 사용자에게 봉인한 키 묶음). 그 사용자의 기기에게만 갑니다.
    Inbox(UserId),
}

impl StreamId {
    /// 부분 전달 스트림인지. 이런 스트림은 gossip에 싣지 않고, 공간·채널 스트림의 부모가 되지 않습니다.
    pub fn is_scoped(&self) -> bool {
        matches!(self, StreamId::KeyGroup(_) | StreamId::Inbox(_))
    }
}

/// 부분 전달 범위. 헤더에 있으면 그 이벤트는 이 범위의 스트림에 속합니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Scope {
    KeyGroup(KeyGroupId),
    Inbox(UserId),
}

impl Scope {
    pub fn stream(self) -> StreamId {
        match self {
            Scope::KeyGroup(g) => StreamId::KeyGroup(g),
            Scope::Inbox(u) => StreamId::Inbox(u),
        }
    }
}

/// 이벤트 헤더. postcard 직렬화는 필드 순서대로(위치 기반)이고 판은 [`EVENT_VERSION`] 하나뿐입니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventHeader {
    pub v: u8,
    pub space: SpaceId,
    pub channel: Option<ChannelId>,
    pub kind: EventKind,
    /// 작성 기기.
    pub author: DeviceId,
    /// 작성 기기의 소유자(신원). 기기가 이 사용자의 기기 목록에 있어야 유효합니다.
    pub user: UserId,
    /// 같은 스트림의 부모 이벤트(정렬됨, 최대 [`MAX_PARENTS`]).
    pub parents: Vec<EventId>,
    /// 논리 시계(공간 전체 기준). 부모들의 최댓값 + 1 이상.
    pub lamport: u64,
    /// 작성자 벽시계. 표시용이며 순서 결정에는 쓰지 않습니다.
    pub created_at: Timestamp,
    pub key: KeyRef,
    pub body_hash: [u8; 32],
    pub body_len: u32,
    /// 부분 전달 범위. 있으면 이 이벤트는 그 범위의 스트림에 속하고 부모도 그 스트림에서만 고릅니다.
    pub scope: Option<Scope>,
    /// 부분 전달 이벤트를 쓸 때 본 공간 스트림 머리(1~[`MAX_STATE_ANCHOR`]개, 정렬). 권한은 이 지점의 상태로
    /// 검사합니다. 부모가 아니므로 공간 스트림 DAG에는 아무 영향이 없습니다.
    pub state_anchor: Vec<EventId>,
}

impl EventHeader {
    /// 짧은 서명(공개키 없음)을 쓰는 이벤트인지: 채널 스트림. 다른 스트림은 긴 서명만 받습니다.
    pub fn wants_slim_sig(&self) -> bool {
        matches!(self.stream(), StreamId::Channel(_))
    }

    pub fn stream(&self) -> StreamId {
        if let Some(scope) = self.scope {
            return scope.stream();
        }
        match (self.kind.lane(), self.channel) {
            (Lane::Message | Lane::Key, Some(c)) => StreamId::Channel(c),
            _ => StreamId::Space,
        }
    }

    pub fn canonical_bytes(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("헤더 직렬화는 실패하지 않음")
    }

    /// AEAD 연관 데이터: 본문 해시·길이를 뺀 헤더. 암호문을 이 문맥에 묶습니다.
    pub fn aead_ad(&self) -> Vec<u8> {
        let mut h = self.clone();
        h.body_hash = [0; 32];
        h.body_len = 0;
        let mut out = b"yangpa/v1/aead\0".to_vec();
        out.extend_from_slice(&h.canonical_bytes());
        out
    }

    /// 구조 검사(서명 검증 전에 싸게 거를 수 있는 것들).
    pub fn check_shape(&self) -> Result<(), CoreError> {
        if self.v != EVENT_VERSION {
            return Err(CoreError::UnsupportedVersion(self.v));
        }
        if self.parents.len() > MAX_PARENTS {
            return Err(CoreError::LimitExceeded("부모 수"));
        }
        if !self.parents.windows(2).all(|w| w[0] < w[1]) {
            return Err(CoreError::InvalidEncoding);
        }
        if self.body_len as usize > self.kind.max_body_len() {
            return Err(CoreError::LimitExceeded("본문 크기"));
        }
        if self.kind.requires_channel() && self.channel.is_none() {
            return Err(CoreError::InvalidEncoding);
        }
        // 키 갱신 본문은 늘 상태 키(크루 그룹은 갱신 전 공간 상태 키, 비공개 그룹은 기대는 크루 상태의 상태 키)로
        // 암호화하고 채널에 매이지 않습니다.
        let key_ok = match self.kind {
            EventKind::KeyUpdate => matches!(self.key, KeyRef::State(_)) && self.channel.is_none(),
            _ => true,
        };
        if !key_ok {
            return Err(CoreError::InvalidEncoding);
        }
        self.check_scope()
    }

    /// 부분 전달 범위 규칙. 범위가 있으면 기준점도 있어야 하고(권한을 검사할 지점), 없으면 기준점도 없어야
    /// 합니다. 상태 레인은 모두가 같은 상태를 얻어야 하므로 범위를 가질 수 없습니다.
    fn check_scope(&self) -> Result<(), CoreError> {
        let Some(scope) = self.scope else {
            return if self.state_anchor.is_empty() {
                Ok(())
            } else {
                Err(CoreError::InvalidEncoding)
            };
        };
        if self.state_anchor.is_empty() || self.state_anchor.len() > MAX_STATE_ANCHOR {
            return Err(CoreError::LimitExceeded("상태 기준점 수"));
        }
        if !self.state_anchor.windows(2).all(|w| w[0] < w[1]) {
            return Err(CoreError::InvalidEncoding);
        }
        if self.channel.is_some() {
            return Err(CoreError::InvalidEncoding);
        }
        // 크루 그룹(ID = 공간 ID)은 모든 크루 기기가 받으므로 공간 스트림에 둡니다. 같은 그룹이 두 스트림에 갈라지지 않게.
        if scope == Scope::KeyGroup(KeyGroupId::crew(&self.space)) {
            return Err(CoreError::InvalidEncoding);
        }
        let ok = match scope {
            // 받은편지함: 그 사용자에게 봉인한 키 묶음만.
            Scope::Inbox(_) => self.kind == EventKind::KeyShare && self.key == KeyRef::Sealed,
            // 키 그룹: 그 그룹의 키 묶음(그룹 상태 키나 봉인)과 키 합의 이벤트만.
            Scope::KeyGroup(_) => match self.kind.lane() {
                Lane::Key => matches!(self.key, KeyRef::State(_) | KeyRef::Sealed),
                Lane::KeyAgreement => true,
                Lane::Message | Lane::State => false,
            },
        };
        if ok {
            Ok(())
        } else {
            Err(CoreError::InvalidEncoding)
        }
    }
}

/// 서명된 이벤트. `body`가 `None`이면 본문을 받지 않았거나 삭제된 상태입니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedEvent {
    pub header: EventHeader,
    pub sig: Sig,
    #[serde(with = "opt_bytes")]
    pub body: Option<Vec<u8>>,
}

impl SignedEvent {
    /// 헤더를 완성하고 서명합니다. `header.body_hash`/`body_len`은 여기서 채웁니다.
    pub fn sign(
        mut header: EventHeader,
        body: Vec<u8>,
        device: &SigningKeypair,
    ) -> Result<Self, CoreError> {
        if body.len() > header.kind.max_body_len() {
            return Err(CoreError::LimitExceeded("본문 크기"));
        }
        header.parents.sort();
        header.parents.dedup();
        header.body_hash = *blake3::hash(&body).as_bytes();
        header.body_len = body.len() as u32;
        header.check_shape()?;
        if header.author.as_bytes() != &device.public() {
            return Err(CoreError::BadSignature);
        }
        // 채널 스트림은 작성 기기가 멤버의 기기라 받는 쪽이 키를 알아 짧은 서명, 나머지(상태·키 합의·부분 전달)는 키를
        // 모르는 기기(가입자 등)도 먼저 검증해야 해서 공개키를 싣습니다.
        let sig = if header.wants_slim_sig() {
            device.sign_slim(SignDomain::Event, &header.canonical_bytes())
        } else {
            device.sign(SignDomain::Event, &header.canonical_bytes())
        };
        Ok(SignedEvent {
            header,
            sig,
            body: Some(body),
        })
    }

    pub fn id(&self) -> EventId {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"yangpa/v2/event-id\0");
        hasher.update(&self.header.canonical_bytes());
        hasher.update(&self.sig.fingerprint());
        EventId(*hasher.finalize().as_bytes())
    }

    /// 서명과(본문이 있으면) 본문 해시를 검증합니다. 작성 기기가 사용자 기기 목록에
    /// 있는지, 작성 시점에 권한이 있었는지는 호출자(동기화·상태 해소)가 확인합니다. 짧은 서명은 키가 없어 거절하므로
    /// [`Self::verify_with`]를 씁니다.
    pub fn verify(&self) -> Result<(), CoreError> {
        self.check_integrity()?;
        verify(
            self.header.author.as_bytes(),
            SignDomain::Event,
            &self.header.canonical_bytes(),
            &self.sig,
        )
    }

    /// 작성 기기의 서명 공개키로 검증합니다(짧은 서명·긴 서명 모두).
    pub fn verify_with(&self, keys: &DeviceSignKeys) -> Result<(), CoreError> {
        self.check_integrity()?;
        crate::sign::verify_with_keys(
            self.header.author.as_bytes(),
            &keys.ed_pk,
            &keys.pq_pk,
            SignDomain::Event,
            &self.header.canonical_bytes(),
            &self.sig,
        )
    }

    /// 서명 없이 볼 수 있는 것: 헤더 모양, 서명 길이가 스트림에 맞는지, 본문 해시.
    pub fn check_integrity(&self) -> Result<(), CoreError> {
        self.header.check_shape()?;
        if self.sig.is_slim() && !self.header.wants_slim_sig() {
            return Err(CoreError::BadSignature);
        }
        if let Some(body) = &self.body
            && (body.len() != self.header.body_len as usize
                || *blake3::hash(body).as_bytes() != self.header.body_hash)
        {
            return Err(CoreError::BadSignature);
        }
        Ok(())
    }

    /// 본문 없이 헤더만 남긴 사본(삭제·미수신 표시용).
    pub fn without_body(&self) -> SignedEvent {
        SignedEvent {
            header: self.header.clone(),
            sig: self.sig.clone(),
            body: None,
        }
    }
}

mod opt_bytes {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
        v.as_ref().map(|b| serde_bytes::Bytes::new(b)).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
        Ok(Option::<serde_bytes::ByteBuf>::deserialize(d)?.map(|b| b.into_vec()))
    }
}

/// 동시 이벤트의 결정론적 정렬 키: (논리 시계, 이벤트 ID).
pub fn order_key(e: &SignedEvent) -> (u64, EventId) {
    (e.header.lamport, e.id())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn header(dev: &SigningKeypair, user: &SigningKeypair) -> EventHeader {
        EventHeader {
            v: EVENT_VERSION,
            space: SpaceId::random(),
            channel: Some(ChannelId::random()),
            kind: EventKind::Message,
            author: DeviceId(dev.public()),
            user: UserId(user.public()),
            parents: vec![],
            lamport: 1,
            created_at: Timestamp::now(),
            key: KeyRef::Record {
                root: ChannelRootId::random(),
            },
            body_hash: [0; 32],
            body_len: 0,
            scope: None,
            state_anchor: Vec::new(),
        }
    }

    /// 받은편지함 키 묶음 헤더(형식 검사를 통과하는 것).
    fn inbox_header(dev: &SigningKeypair, user: &SigningKeypair) -> EventHeader {
        EventHeader {
            channel: None,
            kind: EventKind::KeyShare,
            key: KeyRef::Sealed,
            scope: Some(Scope::Inbox(UserId::random())),
            state_anchor: vec![EventId::hash_of(b"a")],
            ..header(dev, user)
        }
    }

    /// 1판 헤더 배치(필드 12개, `scope`·`state_anchor` 없음). 2판만 받는다는 규칙을 바이트로 확인하려고 둡니다.
    #[derive(Serialize, Deserialize)]
    struct V1Header {
        v: u8,
        space: SpaceId,
        channel: Option<ChannelId>,
        kind: EventKind,
        author: DeviceId,
        user: UserId,
        parents: Vec<EventId>,
        lamport: u64,
        created_at: Timestamp,
        key: KeyRef,
        body_hash: [u8; 32],
        body_len: u32,
    }

    /// 헤더 배치는 필드 14개 하나뿐입니다(2판에서 정해지고 3판은 키 표시 변형만 바뀜). 1판 배치는 헤더로 풀리지 않고,
    /// 1판으로 서명된 이벤트는 풀리더라도 검증에서 거릅니다(4단계 기기 항목 변경으로 1판 본문은 어차피 풀리지 않아 예전
    /// 읽기 길을 두지 않음).
    #[test]
    fn only_current_header_layout() {
        let dev = SigningKeypair::generate();
        let user = SigningKeypair::generate();
        let h = header(&dev, &user);
        let v1 = V1Header {
            v: 1,
            space: h.space,
            channel: h.channel,
            kind: h.kind,
            author: h.author,
            user: h.user,
            parents: vec![],
            lamport: 7,
            created_at: h.created_at,
            key: h.key,
            body_hash: *blake3::hash(b"x").as_bytes(),
            body_len: 1,
        };
        let v1_bytes = postcard::to_stdvec(&v1).unwrap();
        assert!(postcard::from_bytes::<EventHeader>(&v1_bytes).is_err());
        // 1판으로 서명된 저장 형식(헤더 ‖ 서명 ‖ 본문): 서명 바이트를 두 필드로 잘못 읽어 풀리더라도 검증이 거부.
        let sig = dev.sign(SignDomain::Event, &v1_bytes);
        let mut stored = v1_bytes.clone();
        stored.extend(postcard::to_stdvec(&sig).unwrap());
        stored.extend(postcard::to_stdvec(&Some(serde_bytes::Bytes::new(b"x"))).unwrap());
        if let Ok(ev) = postcard::from_bytes::<SignedEvent>(&stored) {
            assert!(ev.verify().is_err());
        }
        // 2판은 두 필드를 실어 1판과 다른 바이트이고 그대로 왕복합니다.
        let v2 = EventHeader {
            parents: vec![],
            lamport: 7,
            body_hash: v1.body_hash,
            body_len: 1,
            ..h
        };
        let v2_bytes = v2.canonical_bytes();
        assert_ne!(v2_bytes, v1_bytes);
        // 앞 12개 필드는 판 바이트만 다르고, 범위·기준점이 뒤에 붙습니다.
        assert_eq!(v2_bytes[0], EVENT_VERSION);
        assert!(v2_bytes[1..].starts_with(&v1_bytes[1..]));
        assert_eq!(postcard::from_bytes::<EventHeader>(&v2_bytes).unwrap(), v2);
        // 모르는 형식(다음 판)과 앞 판(키 표시 변형 번호가 다름)은 거부.
        for v in [2, EVENT_VERSION + 1] {
            let mut other = v2.clone();
            other.v = v;
            assert!(matches!(
                other.check_shape(),
                Err(CoreError::UnsupportedVersion(x)) if x == v
            ));
        }
    }

    #[test]
    fn scope_and_anchor_shape() {
        let dev = SigningKeypair::generate();
        let user = SigningKeypair::generate();
        let ok = inbox_header(&dev, &user);
        ok.check_shape().unwrap();
        let Some(Scope::Inbox(u)) = ok.scope else {
            unreachable!()
        };
        assert_eq!(ok.stream(), StreamId::Inbox(u));
        assert!(ok.stream().is_scoped());
        let ev = SignedEvent::sign(ok.clone(), vec![1], &dev).unwrap();
        let back: SignedEvent = postcard::from_bytes(&postcard::to_stdvec(&ev).unwrap()).unwrap();
        assert_eq!(back.id(), ev.id());
        back.verify().unwrap();

        let a = EventId::hash_of(b"1");
        let b = EventId::hash_of(b"2");
        let (lo, hi) = if a < b { (a, b) } else { (b, a) };
        let bad = |f: &dyn Fn(&mut EventHeader)| {
            let mut h = ok.clone();
            f(&mut h);
            h.check_shape().is_err()
        };
        // 범위가 있으면 기준점이 있어야 하고, 없으면 기준점도 없어야 합니다.
        assert!(bad(&|h| h.state_anchor.clear()));
        assert!(bad(&|h| h.scope = None));
        // 기준점은 최대 4개, 정렬·중복 없음.
        assert!(bad(&|h| {
            h.state_anchor = (0u8..5).map(|i| EventId::hash_of(&[i])).collect();
            h.state_anchor.sort();
        }));
        assert!(bad(&|h| h.state_anchor = vec![hi, lo]));
        assert!(bad(&|h| h.state_anchor = vec![lo, lo]));
        assert!(!bad(&|h| h.state_anchor = vec![lo, hi]));
        // 받은편지함: 봉인한 키 묶음만, 채널 없음.
        assert!(bad(&|h| h.channel = Some(ChannelId::random())));
        assert!(bad(&|h| h.key = KeyRef::State(StateKeyId::random())));
        assert!(bad(&|h| {
            h.kind = EventKind::KeyUpdate;
            h.key = KeyRef::State(StateKeyId::random());
        }));
        // 상태 레인은 범위를 가질 수 없습니다.
        for kind in [EventKind::MemberJoin, EventKind::RoleCreate] {
            assert!(bad(&|h| {
                h.kind = kind;
                h.key = KeyRef::State(StateKeyId::random());
            }));
            assert!(bad(&|h| {
                h.kind = kind;
                h.scope = Some(Scope::KeyGroup(KeyGroupId::random()));
            }));
        }
        // 키 그룹: 키 레인(그룹 상태 키·봉인)과 키 합의 레인만.
        let g = Some(Scope::KeyGroup(KeyGroupId::random()));
        assert!(!bad(&|h| h.scope = g));
        assert!(!bad(&|h| {
            h.scope = g;
            h.key = KeyRef::State(StateKeyId::random());
        }));
        assert!(!bad(&|h| {
            h.scope = g;
            h.kind = EventKind::KeyUpdate;
            h.key = KeyRef::State(StateKeyId::random());
        }));
        // 키 갱신은 상태 키로만.
        assert!(bad(&|h| {
            h.scope = g;
            h.kind = EventKind::KeyUpdate;
            h.key = KeyRef::Sealed;
        }));
        assert!(bad(&|h| {
            h.scope = g;
            h.kind = EventKind::Message;
            h.key = KeyRef::Record {
                root: ChannelRootId::random(),
            };
        }));
        assert!(bad(&|h| {
            h.scope = g;
            h.key = KeyRef::Volatile { generation: 1 };
        }));
        if let Some(Scope::KeyGroup(gid)) = g {
            let mut h = ok.clone();
            h.scope = g;
            assert_eq!(h.stream(), StreamId::KeyGroup(gid));
        }
        // 크루 그룹은 키 그룹 스트림에 실을 수 없습니다(공간 스트림).
        assert!(bad(
            &|h| h.scope = Some(Scope::KeyGroup(KeyGroupId::crew(&h.space)))
        ));
        // 범위 없는 이벤트는 그대로 공간·채널 스트림입니다.
        let plain = header(&dev, &user);
        assert_eq!(plain.stream(), StreamId::Channel(plain.channel.unwrap()));
        assert!(!plain.stream().is_scoped());
    }

    #[test]
    fn sign_verify_and_id_stability() {
        let dev = SigningKeypair::generate();
        let user = SigningKeypair::generate();
        let keys = DeviceSignKeys::of(&dev);
        let ev = SignedEvent::sign(header(&dev, &user), b"ciphertext".to_vec(), &dev).unwrap();
        // 채널 스트림은 짧은 서명: 키 없이는 검증되지 않고 작성 기기 키로 검증됩니다.
        assert_eq!(ev.sig.as_bytes().len(), Sig::SLIM_LEN);
        assert!(ev.verify().is_err());
        ev.verify_with(&keys).unwrap();
        assert!(ev.verify_with(&DeviceSignKeys::of(&user)).is_err());
        let id = ev.id();
        // 본문을 버려도 ID와 서명 검증은 유지된다.
        let pruned = ev.without_body();
        assert_eq!(pruned.id(), id);
        pruned.verify_with(&keys).unwrap();
        // 본문 변조는 거부.
        let mut bad = ev.clone();
        bad.body = Some(b"tampered!!".to_vec());
        assert!(bad.verify_with(&keys).is_err());
        // 헤더 변조는 거부.
        let mut bad = ev.clone();
        bad.header.lamport = 9;
        assert!(bad.verify_with(&keys).is_err());
        // 공간 스트림은 긴 서명이고, 짧은 서명을 붙이면 모양에서 거절합니다.
        let mut h = header(&dev, &user);
        h.channel = None;
        h.kind = EventKind::ChannelCreate;
        h.key = KeyRef::State(StateKeyId([0; 16]));
        let st = SignedEvent::sign(h, vec![1], &dev).unwrap();
        assert_eq!(st.sig.as_bytes().len(), Sig::LEN);
        st.verify().unwrap();
        let mut slimmed = st.clone();
        slimmed.sig = dev.sign_slim(SignDomain::Event, &st.header.canonical_bytes());
        assert!(slimmed.verify_with(&keys).is_err());
    }

    #[test]
    fn postcard_roundtrip() {
        let dev = SigningKeypair::generate();
        let user = SigningKeypair::generate();
        let ev = SignedEvent::sign(header(&dev, &user), vec![1, 2, 3], &dev).unwrap();
        let bytes = postcard::to_stdvec(&ev).unwrap();
        let back: SignedEvent = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, ev);
        assert_eq!(back.id(), ev.id());
    }

    #[test]
    fn kind_codes_and_lanes() {
        for (code, kind) in EventKind::TABLE {
            assert_eq!(kind.code(), *code);
            assert_eq!(EventKind::from_code(*code), *kind);
        }
        assert_eq!(EventKind::Message.lane(), Lane::Message);
        assert_eq!(EventKind::KeyShare.lane(), Lane::Key);
        assert_eq!(EventKind::RoleCreate.lane(), Lane::State);
        assert_eq!(EventKind::ChannelTransportSet.code(), 67);
        assert_eq!(EventKind::ChannelTransportSet.lane(), Lane::State);
        assert_eq!(EventKind::RoleDisplaySet.code(), 68);
        assert_eq!(EventKind::RoleDisplaySet.lane(), Lane::State);
        assert_eq!(EventKind::ChannelAnonymousSet.code(), 69);
        assert_eq!(EventKind::ChannelAnonymousSet.lane(), Lane::State);
        assert_eq!(EventKind::CrewServersSet.code(), 70);
        assert_eq!(EventKind::CrewServersSet.lane(), Lane::State);
        assert_eq!(EventKind::ForumFeaturedSet.code(), 71);
        assert_eq!(EventKind::ForumFeaturedSet.lane(), Lane::State);
        assert_eq!(EventKind::ChannelAttachLimitSet.code(), 72);
        assert_eq!(EventKind::ChannelAttachLimitSet.lane(), Lane::State);
        assert_eq!(EventKind::ChannelEditRulesSet.code(), 73);
        assert_eq!(EventKind::ChannelEditRulesSet.lane(), Lane::State);
        assert_eq!(EventKind::EmojiLimitsSet.code(), 79);
        assert_eq!(EventKind::EmojiLimitsSet.lane(), Lane::State);
        assert_eq!(EventKind::MemberBadges.code(), 101);
        assert_eq!(EventKind::MemberBadges.lane(), Lane::State);
        assert_eq!(EventKind::CrewDisband.code(), 102);
        assert_eq!(EventKind::CrewDisband.lane(), Lane::State);
        // 익명 글은 메시지 레인이고 본문 종류는 원래 종류입니다.
        for (anon, base) in [
            (EventKind::AnonMessage, EventKind::Message),
            (EventKind::AnonForumPost, EventKind::ForumPost),
        ] {
            assert_eq!(anon.lane(), Lane::Message);
            assert!(anon.requires_channel());
            assert!(anon.is_anonymous() && !base.is_anonymous());
            assert_eq!(anon.base(), base);
            assert_eq!(base.anonymized(), anon);
            assert_eq!(anon.anonymized(), anon);
        }
        assert_eq!(EventKind::AnonMessage.code(), 7);
        assert_eq!(EventKind::AnonForumPost.code(), 8);
        assert_eq!(EventKind::Edit.anonymized(), EventKind::Edit);
        assert_eq!(EventKind::Reaction.base(), EventKind::Reaction);
        assert_eq!(EventKind::KeyUpdate.code(), 82);
        assert_eq!(EventKind::KeyUpdate.lane(), Lane::KeyAgreement);
        assert_eq!(EventKind::KeyUpdate.max_body_len(), MAX_KEY_UPDATE_BODY_LEN);
        // 옛 MLS 코드는 모르는 종류로 남습니다.
        assert_eq!(EventKind::from_code(80), EventKind::Other(80));
        assert_eq!(EventKind::from_code(81), EventKind::Other(81));
        assert_eq!(EventKind::KeyBackupSet.code(), 75);
        assert_eq!(EventKind::KeyBackupSet.lane(), Lane::State);
        assert_eq!(EventKind::MemberFlair.code(), 100);
        assert_eq!(EventKind::MemberFlair.lane(), Lane::State);
        assert_eq!(EventKind::from_code(999), EventKind::Other(999));
    }

    /// 앞 판 이벤트(1판: 기기 목록 항목에 서명 공개키가 없던 형식, 2판: 에포크·기록 키 표시)는 서명이 맞아도
    /// 머리에서 거릅니다.
    #[test]
    fn previous_event_version_is_rejected() {
        let dev = SigningKeypair::generate();
        let user = SigningKeypair::generate();
        for v in [1, 2] {
            let mut h = header(&dev, &user);
            h.v = v;
            assert!(matches!(
                h.check_shape(),
                Err(CoreError::UnsupportedVersion(x)) if x == v
            ));
            assert!(SignedEvent::sign(h, vec![1], &dev).is_err());
        }
    }

    /// 크루 키 갱신(공간 스트림): 상태 키로 암호화하고 채널이 없어야 합니다.
    #[test]
    fn key_update_shape() {
        let dev = SigningKeypair::generate();
        let user = SigningKeypair::generate();
        let h = EventHeader {
            channel: None,
            kind: EventKind::KeyUpdate,
            key: KeyRef::State(StateKeyId::random()),
            ..header(&dev, &user)
        };
        h.check_shape().unwrap();
        assert_eq!(h.stream(), StreamId::Space);
        for key in [KeyRef::Sealed, KeyRef::Volatile { generation: 1 }] {
            let mut bad = h.clone();
            bad.key = key;
            assert!(bad.check_shape().is_err(), "{key:?}");
        }
        let mut bad = h.clone();
        bad.channel = Some(ChannelId::random());
        assert!(bad.check_shape().is_err());
        // 큰 본문 한도(키 합의 레인)를 씁니다.
        let mut big = h;
        big.body_len = MAX_KEY_UPDATE_BODY_LEN as u32;
        big.check_shape().unwrap();
        big.body_len += 1;
        assert!(big.check_shape().is_err());
    }

    #[test]
    fn wrong_signer_rejected() {
        let dev = SigningKeypair::generate();
        let other = SigningKeypair::generate();
        let user = SigningKeypair::generate();
        assert!(SignedEvent::sign(header(&dev, &user), vec![], &other).is_err());
    }
}
