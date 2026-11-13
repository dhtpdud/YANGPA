//! 상태 해소: 공간 수준 상태·키 합의 이벤트를 결정론적 순서로 적용하며 권한을 검사합니다.
//!
//! 순서 규칙
//! - 부모 링크로 위상 정렬(Kahn)합니다. 입력에 없는 부모는 충족된 것으로 봅니다(빈 구간 허용).
//! - 동시에 준비된 이벤트 사이에서는 (그 시점까지 적용된 상태에서 작성자의 역할 순위 내림차순,
//!   논리 시계 오름차순, 이벤트 ID 오름차순)으로 고릅니다. 입력 순서와 무관하게 결과가 같습니다.
//! - 각 이벤트는 '그때까지 적용된 상태'로 검사해 통과할 때만 반영합니다. 거부된 이벤트도 자식의
//!   준비 조건은 풀어 줍니다(거부는 그 이벤트의 효과만 없앰).
//!
//! 같은 공간 스트림에는 본문을 풀 수 없거나 해석하지 않는 이벤트(공간 수준 `KeyShare`)도 섞여 있습니다.
//! 이것들을 빼면 인과 관계가 끊겨, 나중 이벤트가 앞 이벤트보다 먼저 적용될 수 있습니다. 그래서 그런
//! 이벤트는 [`LinkEvent`](헤더의 ID·부모·논리 시계만)로 넘기면 순서 연결에만 쓰고 적용하지 않습니다.
//! 키 레인 [`DecodedEvent`]도 같은 방식으로 다룹니다.

mod ancestry;
mod authorize;
mod facts;
mod group;
mod machine;
#[cfg(test)]
mod tests;

use std::cell::OnceCell;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use thiserror::Error;
use yangpa_core::body::EventBody;
use yangpa_core::checkpoint::{
    DelegationCert, DeviceLeafProof, FRONTIER_DEPTH, FrontierEntry, KeyHead, MAX_FRONTIER,
    MAX_VALID_STATE_KEYS, MerkleProof, OwnershipCert, RESOLVER_VERSION, SignedCheckpoint,
};
use yangpa_core::device::MemberDevices;
use yangpa_core::event::{EventHeader, EventKind, KeyRef, Lane};
use yangpa_core::ids::{CheckpointId, DeviceId, EventId, RoleId, SpaceId, StateKeyId, UserId};
use yangpa_core::model::{Permissions, SharedMap, SpaceState};

use ancestry::Dag;
#[cfg(test)]
use ancestry::NoDag;
pub use authorize::{
    authorize_message, authorize_revoked_device_message, authorize_scoped, state_key_handover_only,
};
pub use facts::UpdateKind;
use facts::{Stored, StoredBody};
pub use group::{
    AnchorPlace, ChainBase, ChainChange, Equivocation, GroupChain, GroupTip, KeyPoint, KeyPoints,
};
pub use machine::CheckpointBase;
pub(crate) use machine::Machine;
pub(crate) use machine::check_owner_proof;
pub use machine::keys::{CrewKeys, KeyView};
use machine::{Effect, RankChange};

/// 본문에 실린 서명(공간 생성의 기기 목록·프로필)의 검증 결과 기억. 서명 유효성은 상태와 무관하므로 이벤트마다
/// 처음 필요할 때 한 번만 검증합니다(재해소 때마다 혼합 서명 검증을 반복하지 않게). 필요해질 때까지 미루므로, 다른
/// 이유로 거부될 이벤트의 서명은 검증하지 않습니다. 가입·기기 목록 갱신은 받을 때 검사 결과만 뽑아 둡니다([`facts`]).
#[derive(Clone, Debug, Default)]
pub(crate) struct SigMemo {
    lists: OnceCell<bool>,
    profile: OnceCell<bool>,
    /// 소유권 이전·위임 인증서.
    cert: OnceCell<bool>,
    /// 체크포인트 본문 서명·모양.
    checkpoint: OnceCell<bool>,
}

impl SigMemo {
    /// 공간 생성 본문의 기기 목록 서명이 모두 유효한지(없으면 참).
    pub(crate) fn lists_ok(&self, body: &EventBody) -> bool {
        *self.lists.get_or_init(|| match body {
            EventBody::SpaceCreate { devices, .. } => devices.iter().all(|d| d.verify().is_ok()),
            _ => true,
        })
    }

    /// 소유권 이전·체크포인트 위임 인증서의 서명이 유효한지(없으면 참).
    pub(crate) fn cert_ok(&self, body: &EventBody) -> bool {
        *self.cert.get_or_init(|| match body {
            EventBody::OwnershipTransfer { cert, .. } => cert.verify().is_ok(),
            EventBody::CheckpointDelegation(d) => d.verify().is_ok(),
            _ => true,
        })
    }

    /// 체크포인트 본문의 모양과 서명이 맞는지.
    pub(crate) fn checkpoint_ok(&self, c: &SignedCheckpoint) -> bool {
        *self.checkpoint.get_or_init(|| c.verify().is_ok())
    }

    /// 프로필·뱃지 카드 본문의 서명이 유효한지(없으면 참).
    pub(crate) fn profile_ok(&self, body: &EventBody) -> bool {
        *self.profile.get_or_init(|| match body {
            EventBody::MemberProfile {
                profile: Some(p), ..
            } => p.verify().is_ok(),
            EventBody::MemberBadges { card } => card.verify().is_ok(),
            _ => true,
        })
    }
}

/// 1대1 스페이스의 @everyone 권한. 두 친구가 같은 권한으로 채널·이모지·음성·공간 설정을 관리합니다.
/// 역할·추방·초대·남의 메시지 관리는 없고(크루로 전환해야 생김), 1대1에서는 역할 관리·추방·초대
/// 이벤트 자체를 거부합니다.
pub const FRIEND_SPACE_EVERYONE: Permissions = Permissions::DEFAULT_EVERYONE
    .union(Permissions::USE_EXTERNAL_EMOJI)
    .union(Permissions::MANAGE_CHANNELS)
    .union(Permissions::MANAGE_EMOJI)
    .union(Permissions::MANAGE_VOICE)
    .union(Permissions::MANAGE_SPACE);

/// 1대1 → 크루 전환 때 수락한 사람에게 주는 '관리자' 역할의 순위.
pub const ADMIN_ROLE_RANK: u32 = 100;
/// '관리자' 역할 이름.
pub const ADMIN_ROLE_NAME: &str = "관리자";

/// 1대1 → 크루 전환 때 만드는 '관리자' 역할 ID. 모든 노드가 같은 ID를 얻도록 공간 ID에서 파생합니다.
pub fn admin_role_id(space: &SpaceId) -> RoleId {
    let mut h = blake3::Hasher::new();
    h.update(b"yangpa/v1/admin-role");
    h.update(space.as_bytes());
    let mut id = [0u8; 16];
    id.copy_from_slice(&h.finalize().as_bytes()[..16]);
    RoleId(id)
}

/// 상태 이벤트·메시지 검증 한도. 네트워크 입력의 크기와 개수를 묶어 상태가 무한히 커지지 않게 합니다.
pub mod limits {
    /// 공간·채널·역할·태그 이름 최대 글자 수.
    pub const MAX_NAME_CHARS: usize = 100;
    pub const MAX_NICKNAME_CHARS: usize = 64;
    pub const MAX_TOPIC_CHARS: usize = 1024;
    pub const MAX_DESCRIPTION_CHARS: usize = 1024;
    pub const MAX_REASON_CHARS: usize = 512;
    /// 크루 멤버 수 상한. 키 트리 깊이 상한(2^17칸)에 사용자당 기기 2대가 들어가는 값입니다.
    pub const MAX_MEMBERS: usize = 65_536;
    pub const MAX_BANS: usize = 65_536;
    /// 밴과 함께 지울 최근 메시지 구간의 최대 길이(밀리초, 7일).
    pub const MAX_BAN_DELETE_WINDOW_MS: u64 = 7 * 24 * 60 * 60 * 1000;
    pub const MAX_CHANNELS: usize = 500;
    pub const MAX_ROLES: usize = 250;
    pub const MAX_INVITES: usize = 1000;
    pub const MAX_EMOJIS: usize = yangpa_core::model::MAX_EMOJIS;
    pub const MAX_OVERWRITES: usize = 100;
    pub const MAX_FORUM_TAGS: usize = 20;
    /// 기기 목록 항목 수(폐기된 기기 포함).
    pub const MAX_DEVICE_ENTRIES: usize = 64;
    /// 소유자(`u32::MAX`)가 항상 가장 높도록 역할 순위는 이 값까지만.
    pub const MAX_ROLE_RANK: u32 = u32::MAX - 1;
    pub const MAX_MESSAGE_CHARS: usize = 8000;
    pub const MAX_ATTACHMENTS: usize = 10;
    pub const MAX_EMBEDS: usize = 10;
    pub const MAX_MENTIONS: usize = 100;
    pub const MAX_THUMBNAIL_BYTES: usize = 64 * 1024;
    pub const MAX_FILE_NAME_CHARS: usize = 255;
    pub const MAX_MIME_CHARS: usize = 127;
    pub const MAX_URL_CHARS: usize = 2048;
    pub const MAX_EMBED_TEXT_CHARS: usize = 4096;
    pub const MAX_UNICODE_EMOJI_BYTES: usize = 64;
    pub const MAX_POST_TAGS: usize = 5;
    pub const MAX_KEYS_PER_SHARE: usize = 1024;
}

/// 해소기가 상태에 거는 한도 가운데 바꿀 수 있는 것. 기본값은 [`limits`]의 상수와 같습니다. 규모 시뮬레이션이
/// 상수를 올리기 전에 큰 크루를 재 보려고 씁니다(모든 노드가 같은 한도를 써야 결과가 같으므로 데몬은 기본값만 씀).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// 크루 멤버 수 상한.
    pub max_members: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_members: limits::MAX_MEMBERS,
        }
    }
}

/// 복호화가 끝난 이벤트(상태 해소 입력).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedEvent {
    pub id: EventId,
    pub header: EventHeader,
    pub body: EventBody,
}

/// 본문 없이 순서 연결에만 쓰는 같은 스트림의 이벤트(공간 수준 `KeyShare` 등).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkEvent {
    pub id: EventId,
    pub parents: Vec<EventId>,
    pub lamport: u64,
    /// 본문을 풀면 상태에 영향을 주는 이벤트(상태·키 합의 레인)인데 아직 풀지 못해 순서 연결로만 들어 있음(키를 기다림).
    /// [`Resolver::state_at`]이 빈 구간을 허용하지 않으면 이런 조상이 풀릴 때까지 기다립니다.
    pub undecoded: bool,
}

impl LinkEvent {
    pub fn from_header(id: EventId, header: &EventHeader) -> Self {
        LinkEvent {
            id,
            parents: header.parents.clone(),
            lamport: header.lamport,
            undecoded: matches!(header.kind.lane(), Lane::State | Lane::KeyAgreement),
        }
    }

    /// 본문 없이 받아(헤더만 남은 이벤트 등) 풀릴 일이 없는 순서 연결. 해소기처럼 순서에만 쓰고, 기준점 상태도 이것을
    /// 기다리지 않습니다(누구나 헤더만 낸 상태 이벤트 하나로 모든 가입자의 키 묶음을 붙잡지 못하게).
    pub fn without_body(id: EventId, header: &EventHeader) -> Self {
        LinkEvent {
            undecoded: false,
            ..Self::from_header(id, header)
        }
    }
}

/// [`Resolver::state_at`]의 결과.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnchorState {
    /// 기준점이나 그 조상 가운데 모르는 이벤트(`unknown`)나 본문을 아직 풀지 못한 상태 이벤트(`undecoded`)가 있음.
    Incomplete {
        unknown: Vec<EventId>,
        undecoded: Vec<EventId>,
    },
    /// 기준점과 그 조상만으로 해소한 상태(공간 생성 이벤트가 적용되지 않았으면 `None`). `gaps`면 빠진 부모·풀지 못한
    /// 이벤트·모르는 기준점을 빈 구간으로 보고 세운 것이라(`allow_gaps`) 노드마다 다를 수 있습니다. 아니면 조상 집합이
    /// 해시로 고정돼 어느 노드에서나, 언제 다시 세워도 같습니다.
    Ready {
        state: Option<Box<SpaceState>>,
        gaps: bool,
    },
}

/// 이벤트를 거부한 이유.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum AuthError {
    #[error("본문 종류가 헤더와 다름")]
    KindMismatch,
    #[error("이 검사에 맞지 않는 레인의 이벤트")]
    WrongLane,
    #[error("헤더 구조 오류")]
    Malformed,
    #[error("공간 생성 이벤트가 아직 적용되지 않음")]
    NoSpace,
    #[error("공간이 이미 만들어짐")]
    AlreadyCreated,
    #[error("고정된 시작 이벤트가 아닌 공간 생성")]
    NotGenesis,
    #[error("다른 공간의 이벤트")]
    WrongSpace,
    #[error("논리 시계가 부모보다 크지 않음")]
    BadLamport,
    #[error("부모 관계에 순환이 있음")]
    Cycle,
    #[error("작성자가 공간 멤버가 아님")]
    NotMember,
    #[error("작성 기기가 사용자의 활성 기기가 아님")]
    UnknownDevice,
    #[error("권한 없음: {0:?}")]
    MissingPermission(Permissions),
    #[error("역할 순위가 부족함")]
    RankTooLow,
    #[error("소유자만 할 수 있음")]
    NotOwner,
    #[error("커밋 확정 기기가 아님")]
    NotCommitter,
    #[error("커밋 확정 기기가 될 수 없는 기기")]
    InvalidCommitter,
    #[error("소유자는 이 작업의 대상이 될 수 없음")]
    OwnerProtected,
    #[error("@everyone 역할은 지우거나 이름·순위를 바꿀 수 없음")]
    EveryoneProtected,
    #[error("이 공간 종류에서는 할 수 없음")]
    WrongSpaceKind,
    #[error("없는 채널")]
    UnknownChannel,
    #[error("이미 있거나 지운 채널 ID")]
    ChannelExists,
    #[error("없는 역할")]
    UnknownRole,
    #[error("이미 있거나 지운 역할 ID")]
    RoleExists,
    #[error("멤버가 아닌 대상")]
    UnknownMember,
    #[error("이미 멤버임")]
    AlreadyMember,
    #[error("이 크루에서 밴된 사용자")]
    Banned,
    #[error("밴 목록에 없는 사용자")]
    NotBanned,
    #[error("쓸 수 없는 초대(없음·만료·횟수 초과)")]
    InvalidInvite,
    #[error("이미 있는 초대")]
    InviteExists,
    #[error("없는 초대")]
    UnknownInvite,
    #[error("없는 이모지")]
    UnknownEmoji,
    #[error("이미 있는 이모지")]
    EmojiExists,
    #[error("다른 공간 이모지 사용이 꺼져 있음")]
    ExternalEmojiDisabled,
    #[error("잘못된 기기 목록")]
    BadDeviceList,
    #[error("기기 목록 버전이 최신보다 낮음")]
    StaleDeviceList,
    #[error("잘못된 프로필")]
    BadProfile,
    #[error("크루 전환 요청이 없음")]
    NoPendingConversion,
    #[error("상대의 크루 전환 요청이 대기 중")]
    ConversionPending,
    #[error("같은 순번의 키 갱신이 이미 있음")]
    DuplicateCommit,
    #[error("자기 글만 고칠 수 있음")]
    NotAuthor,
    #[error("대상 메시지의 작성자를 아직 모름")]
    UnknownTarget,
    #[error("키 종류가 채널 보안 등급과 맞지 않음")]
    WrongKey,
    #[error("허용 범위를 넘음: {0}")]
    Limit(&'static str),
    #[error("잘못된 값: {0}")]
    Invalid(&'static str),
    #[error("체크포인트 경계 아래를 부모로 둔 늦은 이벤트(지금 머리에 다시 서명해야 함)")]
    LateEvent,
    #[error("체크포인트 경계 아래의 이벤트(효과 없음)")]
    BelowCheckpoint,
    #[error("체크포인트가 은퇴시킨 상태 키(지금 키로 다시 암호화해야 함)")]
    RetiredStateKey,
    #[error("지금 바탕을 잇지 않는 체크포인트")]
    StaleCheckpoint,
    #[error("체크포인트 확인 실패: {0}")]
    InvalidCheckpoint(&'static str),
    #[error("더 새 해소기 판으로 만든 체크포인트(업데이트 필요)")]
    NewerResolver,
    #[error("이미 낸 체크포인트 이견")]
    DuplicateDispute,
    #[error("기준점 상태를 아직 모름")]
    AnchorUnknown,
}

/// 상태 해소 결과.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolved {
    /// 공간 생성 이벤트가 적용되지 않았으면 `None`.
    pub state: Option<SpaceState>,
    /// 적용된 이벤트(적용 순서).
    pub applied: Vec<EventId>,
    /// 거부된 이벤트와 이유(처리 순서).
    pub rejected: Vec<(EventId, AuthError)>,
    /// 데몬이 처리할 키 이벤트(적용 순서): 다 모여 받아들인 크루
    /// `KeyUpdate`(마지막 조각 이벤트), `KeyBackupSet`.
    pub key_events: Vec<EventId>,
    /// 멤버별로 가장 최근에 멤버가 된 이벤트의 논리 시계(시더의 '입장 이전 기록' 판단용).
    pub member_since: SharedMap<UserId, u64>,
    /// 적용된 공간 생성 이벤트. 데몬은 이것을 저장해 두고 [`Resolver::with_genesis`]로 고정할 수 있습니다.
    pub genesis: Option<EventId>,
    /// 크루 키 사슬과 기기 주인(공간이 없으면 `None`).
    pub keys: Option<KeyView>,
}

/// 이벤트들의 상태를 처음부터 해소합니다. 입력 순서와 무관하게 결과가 같습니다.
///
/// 상태·키 합의 레인 이벤트를 넘깁니다. 키 레인 이벤트는 순서 연결에만 쓰고, 메시지 레인 이벤트는
/// [`AuthError::WrongLane`]으로 거부합니다.
pub fn resolve(events: &[DecodedEvent]) -> Resolved {
    resolve_with(events, &[], None)
}

/// [`resolve`]에 순서 연결용 이벤트와 시작 이벤트 고정을 더한 것.
///
/// `genesis`가 있으면 그 ID의 공간 생성 이벤트만 받아들입니다. 멤버 누구나 같은 공간 ID로 가짜
/// 공간 생성 이벤트를 만들 수 있으므로, 데몬은 가입할 때 받은 시작 이벤트를 고정하는 것이 안전합니다.
pub fn resolve_with(
    events: &[DecodedEvent],
    links: &[LinkEvent],
    genesis: Option<EventId>,
) -> Resolved {
    let mut r = match genesis {
        Some(g) => Resolver::with_genesis(g),
        None => Resolver::new(),
    };
    // 빈 해소기에 한 번에 넣으면 처음부터 해소한 것과 같습니다.
    r.insert(events.to_vec(), links.to_vec());
    r.resolved()
}

/// 정렬 엔진이 다루는 항목. `event`가 없으면 순서 연결 전용입니다.
#[derive(Clone, Copy)]
struct Item<'a> {
    id: EventId,
    parents: &'a [EventId],
    lamport: u64,
    event: Option<(&'a Stored, &'a SigMemo)>,
    /// 해소기 항목 번호(처리 기록용).
    entry: usize,
}

/// 키 레인 이벤트는 상태에 영향이 없으므로 순서 연결에만 씁니다.
fn is_link_only(e: &DecodedEvent) -> bool {
    e.header.kind.lane() == Lane::Key && e.body.kind().lane() == Lane::Key
}

/// 부분 전달 이벤트(받은편지함·키 그룹)는 순서 연결로도 넣지 않습니다. 일부 노드만 가지므로, 넣으면 그 이벤트를 부모로
/// 둔 상태 이벤트의 처리 순서가 노드마다 갈립니다. 이런 부모는 어디서나 빠진 부모로 다룹니다.
fn is_scoped(h: &EventHeader) -> bool {
    h.scope.is_some()
}

/// 받아들인 바탕과 같은 순번의 다른 체크포인트(서명이 맞는 것). 갈래를 낸 소유자 기기·위임받은 기기의 증거입니다(경계
/// 아래로 늦게 와 판정하지 못한 것도).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointConflict {
    /// 받아들인 바탕 체크포인트 이벤트.
    pub base: EventId,
    /// 진(또는 판정하지 못한) 체크포인트 이벤트.
    pub other: EventId,
    /// 같은 기기가 둘 다 썼는지(둘 다 증거로 남기고 경보).
    pub same_author: bool,
}

#[derive(Clone, Default)]
struct Outcome {
    applied: Vec<EventId>,
    rejected: Vec<(EventId, AuthError)>,
    key_events: Vec<EventId>,
    changed: bool,
    /// 처리하지 못한 항목(순환과 그 자손)이 남음.
    stuck: bool,
    /// 받아들인 체크포인트(차례대로).
    checkpoints: Vec<EventId>,
    /// 더 새 해소기 판의 체크포인트를 봄.
    needs_update: bool,
}

/// [`Outcome`]의 길이들(체크포인트 후보를 시험 해소하고 되돌릴 때).
struct OutcomeLens {
    applied: usize,
    rejected: usize,
    key_events: usize,
    checkpoints: usize,
    changed: bool,
    stuck: bool,
    needs_update: bool,
}

impl Outcome {
    fn lens(&self) -> OutcomeLens {
        OutcomeLens {
            applied: self.applied.len(),
            rejected: self.rejected.len(),
            key_events: self.key_events.len(),
            checkpoints: self.checkpoints.len(),
            changed: self.changed,
            stuck: self.stuck,
            needs_update: self.needs_update,
        }
    }

    fn restore(&mut self, l: &OutcomeLens) {
        self.applied.truncate(l.applied);
        self.rejected.truncate(l.rejected);
        self.key_events.truncate(l.key_events);
        self.checkpoints.truncate(l.checkpoints);
        self.changed = l.changed;
        self.stuck = l.stuck;
        self.needs_update = l.needs_update;
    }

    /// `l` 뒤에 더한 결과를 떼어 내고 `l`로 되돌립니다.
    fn take_tail(&mut self, l: &OutcomeLens) -> Outcome {
        let tail = Outcome {
            applied: self.applied.split_off(l.applied),
            rejected: self.rejected.split_off(l.rejected),
            key_events: self.key_events.split_off(l.key_events),
            changed: self.changed,
            stuck: self.stuck,
            checkpoints: self.checkpoints.split_off(l.checkpoints),
            needs_update: self.needs_update,
        };
        self.restore(l);
        tail
    }

    /// [`Outcome::take_tail`]로 떼어 낸 것을 다시 붙입니다.
    fn append(&mut self, tail: &Outcome) {
        self.applied.extend_from_slice(&tail.applied);
        self.rejected.extend(tail.rejected.iter().cloned());
        self.key_events.extend_from_slice(&tail.key_events);
        self.checkpoints.extend_from_slice(&tail.checkpoints);
        self.changed |= tail.changed;
        self.stuck |= tail.stuck;
        self.needs_update |= tail.needs_update;
    }
}

/// 해소 실행 설정.
#[derive(Clone, Copy, Debug, Default)]
struct Ctx {
    pin: Option<EventId>,
    limits: Limits,
}

#[cfg(test)]
thread_local! {
    /// 시험 전용: 적용마다 준비 집합 전체의 정렬 키를 다시 계산합니다(좁힌 재정렬이 빠뜨린 순위 변화가 없는지 비교용).
    static FORCE_RANK_ALL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// 시험 전용: 스냅숏 재생 대신 늘 처음부터 다시 해소합니다(재생 결과와 비교용).
    static NO_REPLAY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 스냅숏 재생을 써도 되는지(시험에서 끌 수 있음).
fn replay_enabled() -> bool {
    #[cfg(test)]
    return !NO_REPLAY.get();
    #[cfg(not(test))]
    true
}

/// (순위 내림차순, 논리 시계, ID, 항목 번호). 항목 번호는 ID가 유일하므로 비교에 영향이 없습니다.
type ReadyKey = (Reverse<u64>, u64, EventId, usize);

fn ready_key(machine: &Option<Machine>, items: &[Item<'_>], i: usize) -> ReadyKey {
    let item = &items[i];
    let rank = match (machine, item.event) {
        (Some(m), Some((e, _))) => m.st.rank_of(&e.header.user),
        _ => 0,
    };
    (Reverse(rank), item.lamport, item.id, i)
}

/// 준비된 항목 대기열: 연결 전용 항목은 상태에 영향이 없어 곧바로 처리하고, 나머지는 우선순위 순.
struct Sched {
    ready: BTreeSet<ReadyKey>,
    links: Vec<usize>,
    /// 준비 집합에 있는 항목의 지금 정렬 키(꺼냈거나 아직 준비되지 않았으면 None).
    keys: Vec<Option<ReadyKey>>,
    /// 작성자 → 준비 집합에 넣은 항목. 꺼낸 항목도 남아 있을 수 있어 `keys`로 거릅니다.
    by_user: HashMap<UserId, Vec<usize>>,
}

impl Sched {
    fn new(n: usize) -> Self {
        Sched {
            ready: BTreeSet::new(),
            links: Vec::new(),
            keys: vec![None; n],
            by_user: HashMap::new(),
        }
    }

    fn push(&mut self, machine: &Option<Machine>, items: &[Item<'_>], i: usize) {
        if let Some((ev, _)) = items[i].event {
            let key = ready_key(machine, items, i);
            self.keys[i] = Some(key);
            self.ready.insert(key);
            self.by_user.entry(ev.header.user).or_default().push(i);
        } else {
            self.links.push(i);
        }
    }

    fn pop(&mut self) -> Option<usize> {
        let (_, _, _, i) = self.ready.pop_first()?;
        self.keys[i] = None;
        Some(i)
    }

    /// 순위가 바뀐 범위의 준비 항목만 정렬 키를 다시 계산합니다.
    fn rekey(&mut self, machine: &Option<Machine>, items: &[Item<'_>], change: RankChange) {
        match change {
            RankChange::None => {}
            RankChange::All => {
                let old = std::mem::take(&mut self.ready);
                for (_, _, _, i) in old {
                    let key = ready_key(machine, items, i);
                    self.keys[i] = Some(key);
                    self.ready.insert(key);
                }
            }
            RankChange::Users(users) => {
                for user in users {
                    let Some(list) = self.by_user.get_mut(&user) else {
                        continue;
                    };
                    list.retain(|&i| self.keys[i].is_some());
                    for &i in list.iter() {
                        let key = ready_key(machine, items, i);
                        if let Some(old) = self.keys[i].replace(key)
                            && old != key
                        {
                            self.ready.remove(&old);
                            self.ready.insert(key);
                        }
                    }
                }
            }
        }
    }
}

/// 실행 중 조상 검사가 보는 DAG: 이번 묶음의 항목, 없으면 묶음 밖(이미 처리된 항목).
struct RunDag<'a, 'b> {
    items: &'b [Item<'a>],
    pos: &'b HashMap<EventId, usize>,
    ext: &'b dyn Dag,
}

impl Dag for RunDag<'_, '_> {
    fn node(&self, id: &EventId) -> Option<(u64, &[EventId])> {
        match self.pos.get(id) {
            Some(&i) => Some((self.items[i].lamport, self.items[i].parents)),
            None => self.ext.node(id),
        }
    }
}

/// 해소기가 들고 있는 항목 전부의 DAG.
struct EntriesDag<'a> {
    entries: &'a [Entry],
    index: &'a HashMap<EventId, usize>,
}

impl Dag for EntriesDag<'_> {
    fn node(&self, id: &EventId) -> Option<(u64, &[EventId])> {
        let e = &self.entries[*self.index.get(id)?];
        Some((e.lamport(), e.parents()))
    }
}

/// 위상 정렬하며 적용합니다. `ext`는 이번 묶음 밖(이미 처리된) 항목의 DAG입니다(부모의 논리 시계, 조상 검사).
/// `rec`이 있으면 처리한 순서와 스냅숏을 남깁니다. `judged`는 경계 판정에서 떨어진 체크포인트의 거부 이유입니다.
///
/// 바탕 체크포인트가 있으면 논리 시계가 경계 F 아래인 이벤트는 늦은 이벤트로 거부합니다(보통 부르는 쪽이 먼저 걸러
/// 내지만 기준점 상태 계산은 그대로 넘김). F 아래 이벤트를 부모로만 둔 이벤트는 거부하지 않습니다: 그 부모는 경계
/// 뒤에서 효과가 없으므로(경계 앞에서 처리됐거나 F 아래로 거부됨), 그 부모를 아는 노드와 모르는 노드(체크포인트로 들어온
/// 멤버, 재시작한 노드, 압축한 노드)가 같은 판정을 합니다. 부모의 논리 시계로 거부하면 이 판정이 노드마다 갈립니다.
fn run(
    items: &[Item<'_>],
    ext: &dyn Dag,
    machine: &mut Option<Machine>,
    ctx: Ctx,
    judged: &HashMap<EventId, AuthError>,
    out: &mut Outcome,
    mut rec: Option<&mut Rec>,
) {
    let floor = machine
        .as_ref()
        .and_then(|m| m.base.as_ref())
        .map_or(0, |b| b.floor);
    let n = items.len();
    let mut pos: HashMap<EventId, usize> = HashMap::with_capacity(n);
    for (i, it) in items.iter().enumerate() {
        pos.insert(it.id, i);
    }
    let mut indeg = vec![0u32; n];
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut bad_lamport = vec![false; n];
    let late: Vec<bool> = items.iter().map(|it| it.lamport < floor).collect();
    let mut parents = Vec::new();
    for (i, it) in items.iter().enumerate() {
        parents.clear();
        parents.extend_from_slice(it.parents);
        parents.sort_unstable();
        parents.dedup();
        for p in &parents {
            if *p == it.id {
                bad_lamport[i] = true;
            } else if let Some(&j) = pos.get(p) {
                indeg[i] += 1;
                children[j].push(i);
                if items[j].lamport >= it.lamport {
                    bad_lamport[i] = true;
                }
            } else if ext.node(p).is_some_and(|(l, _)| l >= it.lamport) {
                bad_lamport[i] = true;
            }
        }
    }
    let dag = RunDag {
        items,
        pos: &pos,
        ext,
    };

    let mut done = vec![false; n];
    let mut sched = Sched::new(n);
    // 이전 해소에서 남은 표시는 이번 준비 집합과 무관합니다.
    if let Some(m) = machine.as_mut() {
        m.take_rank_change();
    }
    for (i, _) in indeg.iter().enumerate().filter(|(_, d)| **d == 0) {
        sched.push(machine, items, i);
    }
    loop {
        while let Some(l) = sched.links.pop() {
            done[l] = true;
            if let Some(r) = rec.as_deref_mut() {
                r.note(items[l].entry, machine, out);
            }
            for &c in &children[l] {
                indeg[c] -= 1;
                if indeg[c] == 0 {
                    sched.push(machine, items, c);
                }
            }
        }
        let Some(i) = sched.pop() else {
            break;
        };
        done[i] = true;
        if let Some((ev, memo)) = items[i].event {
            let created = machine.is_none();
            let result = if bad_lamport[i] {
                Err(AuthError::BadLamport)
            } else if late[i] {
                Err(AuthError::LateEvent)
            } else {
                apply_one(machine, ctx, ev, memo, judged, &dag)
            };
            // 공간이 막 생기면 소유자 순위가 생기므로 전부, 아니면 바뀐 사용자만 다시 정렬합니다.
            let change = match machine.as_mut() {
                Some(m) if created => {
                    m.take_rank_change();
                    RankChange::All
                }
                Some(m) => m.take_rank_change(),
                None => RankChange::None,
            };
            #[cfg(test)]
            let change = if FORCE_RANK_ALL.get() {
                RankChange::All
            } else {
                change
            };
            sched.rekey(machine, items, change);
            match result {
                Ok(eff) => {
                    out.applied.push(ev.id);
                    if eff.key_event {
                        out.key_events.push(ev.id);
                    }
                    out.changed |= eff.changed;
                }
                Err(e) => {
                    tracing::debug!(event = %ev.id.short(), kind = ?ev.header.kind, reason = %e, "상태 이벤트 거부");
                    if e == AuthError::NewerResolver {
                        out.needs_update = true;
                    }
                    out.rejected.push((ev.id, e));
                }
            }
        }
        if let Some(r) = rec.as_deref_mut() {
            r.note(items[i].entry, machine, out);
        }
        for &c in &children[i] {
            indeg[c] -= 1;
            if indeg[c] == 0 {
                sched.push(machine, items, c);
            }
        }
    }

    // 해시로 묶인 실제 이벤트에는 순환이 생길 수 없지만, 잘못된 입력에도 결정론적으로 끝냅니다.
    out.stuck |= done.iter().any(|d| !d);
    let mut rest: Vec<usize> = (0..n)
        .filter(|&i| !done[i] && items[i].event.is_some())
        .collect();
    rest.sort_by_key(|&i| (items[i].lamport, items[i].id));
    for i in rest {
        out.rejected.push((items[i].id, AuthError::Cycle));
    }
}

fn apply_one(
    machine: &mut Option<Machine>,
    ctx: Ctx,
    ev: &Stored,
    memo: &SigMemo,
    judged: &HashMap<EventId, AuthError>,
    dag: &dyn Dag,
) -> Result<Effect, AuthError> {
    if ev.body.kind() != ev.header.kind {
        return Err(AuthError::KindMismatch);
    }
    let lane = ev.header.kind.lane();
    if !matches!(lane, Lane::State | Lane::KeyAgreement) {
        return Err(AuthError::WrongLane);
    }
    ev.header.check_shape().map_err(|_| AuthError::Malformed)?;
    // protocol.md의 KeyRef 표: 상태 레인과 키 갱신(`KeyUpdate`, 갱신 전 상태 키)은 상태 키뿐입니다. 다른 키(특히
    // 봉인 본문)를 받아 주면 공간 구조가 모든 멤버에게 같은 키로 퍼지지 않습니다.
    let KeyRef::State(state_key) = ev.header.key else {
        return Err(AuthError::WrongKey);
    };
    // 체크포인트가 은퇴시킨 키: 체크포인트로 들어온 멤버는 이 키가 없어 풀 수 없습니다(키 갱신도 같음).
    if machine.as_ref().is_some_and(|m| m.is_retired(&state_key)) {
        return Err(AuthError::RetiredStateKey);
    }
    if matches!(&ev.body, StoredBody::Full(b) if matches!(**b, EventBody::SpaceCreate { .. })) {
        if machine.is_some() {
            return Err(AuthError::AlreadyCreated);
        }
        let mut m = Machine::create(ev, ctx.pin, ctx.limits, memo)?;
        m.note_state_key(state_key);
        *machine = Some(m);
        return Ok(Effect::changed());
    }
    if let Some(e) = judged.get(&ev.id) {
        return Err(e.clone());
    }
    let m = machine.as_mut().ok_or(AuthError::NoSpace)?;
    let eff = m.apply(ev, memo, dag)?;
    m.note_state_key(state_key);
    Ok(eff)
}

/// [`Resolver::insert`]가 알려 주는 변화.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResolveUpdate {
    /// 증분이 아닌 해소(스냅숏에서 다시 해소 또는 처음부터)였는지. 그랬다면 이전에 적용됐던 이벤트가 거부로 바뀌었을 수
    /// 있습니다. 어느 쪽이었는지는 [`Resolver::stats`]로 압니다.
    pub full: bool,
    /// 이번에 새로 적용된 이벤트(적용 순서).
    pub applied: Vec<EventId>,
    /// 이번에 새로 거부된 이벤트. 체크포인트 경계 아래로 들어온 이벤트는 [`AuthError::BelowCheckpoint`]입니다.
    pub rejected: Vec<(EventId, AuthError)>,
    /// 전에는 적용됐지만 재해소 뒤 적용되지 않은 이벤트(빈 구간이 채워져 순서가 바뀐 경우).
    pub unapplied: Vec<EventId>,
    /// 이번에 새로 적용된 키 이벤트(적용 순서, [`Resolved::key_events`]와 같은 종류).
    pub key_events: Vec<EventId>,
    /// 해소된 공간 상태가 바뀌었는지.
    pub state_changed: bool,
    /// 이번에 새로 바탕이 된 체크포인트(차례대로).
    pub checkpoints: Vec<EventId>,
    /// 처음 본 겨룬 체크포인트.
    pub conflicts: Vec<CheckpointConflict>,
    /// 더 새 해소기 판의 체크포인트를 봄(따르지 않음: 업데이트 필요).
    pub needs_update: bool,
}

/// 증분·재생·전체 해소 횟수(진단·테스트용).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResolverStats {
    /// 이어서 처리만 한 삽입.
    pub incremental: u64,
    /// 처음부터(창의 바탕부터) 다시 해소한 횟수.
    pub full: u64,
    /// 스냅숏에서 다시 해소한 횟수.
    pub replay: u64,
    /// 스냅숏 재생에서 다시 처리한 항목 수 합.
    pub replayed: u64,
    /// 체크포인트로 RAM에서 버린 항목 수 합.
    pub compacted: u64,
}

/// 이만큼 처리할 때마다 Machine 스냅숏을 남깁니다. 시험에서는 작게 해 재생·솎기 경로를 자주 탑니다.
const SNAPSHOT_EVERY: usize = if cfg!(test) {
    4
} else {
    RELEASE_SNAPSHOT_EVERY
};
const RELEASE_SNAPSHOT_EVERY: usize = 1024;

/// 남기는 스냅숏 수 상한. 넘으면 [`push_snapshot`]이 솎아 최근일수록 촘촘하고 오래될수록 성기게(간격이 끝에서의
/// 거리에 비례하게) 남깁니다. 16개면 처리 기록 300만 항목까지 끝에서 d만큼 앞에서 다시 해소할 때 재생 길이가
/// 2.2(d + [`SNAPSHOT_EVERY`]) 안입니다(약 90만 항목까지는 2배 안, 1000만 항목이면 2.5배; 시험
/// `snapshot_bound_with_release_constants`). 스냅숏 메모리는 docs/performance.md "해소기 스냅숏".
const MAX_SNAPSHOTS: usize = if cfg!(test) { 6 } else { RELEASE_MAX_SNAPSHOTS };
const RELEASE_MAX_SNAPSHOTS: usize = 16;

/// 처리 기록의 `pos`에서 아직 처리하지 않은 항목.
const NOT_DONE: usize = usize::MAX;

/// 처리 위치와 그때까지의 결과 길이.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Mark {
    /// 처리한 항목 수(처리 기록에서의 위치).
    at: usize,
    applied: usize,
    rejected: usize,
    key_events: usize,
}

impl Mark {
    /// 앞부분 `cut`을 버린 뒤의 위치.
    fn minus(self, cut: Mark) -> Mark {
        Mark {
            at: self.at - cut.at,
            applied: self.applied - cut.applied,
            rejected: self.rejected - cut.rejected,
            key_events: self.key_events - cut.key_events,
        }
    }
}

/// `mark.at`개를 처리한 직후의 상태.
#[derive(Clone, Debug)]
struct Snapshot {
    mark: Mark,
    machine: Option<Machine>,
}

/// 받아들인 체크포인트(바탕): 처리 기록에서 그 체크포인트까지 처리한 위치와 그 뒤 상태.
#[derive(Clone, Debug)]
struct BaseSlot {
    mark: Mark,
    /// 체크포인트 항목 번호.
    entry: usize,
    machine: Machine,
}

/// [`run`]이 남기는 처리 순서와 스냅숏.
#[derive(Clone)]
struct Rec {
    /// 이번 실행 앞에 이미 처리된 부분.
    base: Mark,
    /// 이번 실행에서 처리한 항목(해소기 항목 번호, 처리 순서).
    order: Vec<usize>,
    snapshots: Vec<Snapshot>,
    /// 이번 실행에서 받아들인 체크포인트.
    bases: Vec<BaseSlot>,
}

impl Rec {
    fn new(base: Mark, snapshots: Vec<Snapshot>) -> Self {
        Rec {
            base,
            order: Vec::new(),
            snapshots,
            bases: Vec::new(),
        }
    }

    fn mark(&self, out: &Outcome) -> Mark {
        Mark {
            at: self.base.at + self.order.len(),
            applied: self.base.applied + out.applied.len(),
            rejected: self.base.rejected + out.rejected.len(),
            key_events: self.base.key_events + out.key_events.len(),
        }
    }

    /// 항목 `entry`를 처리했습니다(적용·거부 결과까지 `out`에 반영된 뒤).
    fn note(&mut self, entry: usize, machine: &Option<Machine>, out: &Outcome) {
        self.order.push(entry);
        let mark = self.mark(out);
        if mark.at.is_multiple_of(SNAPSHOT_EVERY) {
            push_snapshot(
                &mut self.snapshots,
                Snapshot {
                    mark,
                    machine: machine.clone(),
                },
            );
        }
    }

    /// 체크포인트 `entry`를 바탕으로 받아들였습니다(처리 기록에 넣은 뒤). 그 앞의 스냅숏은 다시 쓰지 않습니다: 바탕 앞에서
    /// 다시 해소해야 하면 창의 바탕부터 합니다.
    fn accept(&mut self, entry: usize, machine: &Machine, out: &Outcome) {
        self.snapshots.clear();
        self.bases.push(BaseSlot {
            mark: self.mark(out),
            entry,
            machine: machine.clone(),
        });
    }
}

/// 스냅숏을 더하고 [`MAX_SNAPSHOTS`]를 넘으면 하나를 솎습니다. 가장 최근 것은 남깁니다. 하나를 지우면 그 앞뒤 스냅숏
/// 사이를 다시 해소할 때 재생 길이와 끝에서의 거리의 비가 많아야 (끝 - 앞)/(끝 - 뒤)가 되므로, 이 비가 가장 작은 것을
/// (같으면 오래된 것을) 지웁니다. 그러면 간격이 끝에서의 거리에 비례하게(기하급수로) 남습니다.
fn push_snapshot(snapshots: &mut Vec<Snapshot>, s: Snapshot) {
    push_snapshot_with(snapshots, s, SNAPSHOT_EVERY, MAX_SNAPSHOTS);
}

/// [`push_snapshot`]에 간격·상한을 넘기는 것(시험이 실제 상수로 솎기를 재 보려고).
fn push_snapshot_with(snapshots: &mut Vec<Snapshot>, s: Snapshot, every: usize, max: usize) {
    snapshots.push(s);
    if snapshots.len() <= max {
        return;
    }
    let end = snapshots.last().map_or(0, |s| s.mark.at) + every;
    // (끝 - 앞, 끝 - 뒤). 맨 앞의 앞은 0(처음부터 해소).
    let span = |i: usize| {
        let before = if i == 0 { 0 } else { snapshots[i - 1].mark.at };
        let after = snapshots[i + 1].mark.at;
        ((end - before) as u128, (end - after) as u128)
    };
    let mut victim = 0;
    for i in 1..snapshots.len() - 1 {
        let ((a, b), (c, d)) = (span(i), span(victim));
        // a/b < c/d
        if a * d < c * b {
            victim = i;
        }
    }
    snapshots.remove(victim);
}

/// 해소기가 들고 있는 항목.
#[derive(Clone, Debug)]
enum Entry {
    Event(Box<(Stored, SigMemo)>),
    /// 본문 없이 순서 연결로만 아는 것.
    Link(LinkEvent),
    /// 체크포인트 경계 아래(또는 경계 위 조상)로 논리 시계만 아는 것. 처리하지 않고(이미 처리된 부모처럼 봄) 자식의
    /// 논리 시계·늦은 이벤트 판정에만 씁니다.
    Floor {
        id: EventId,
        lamport: u64,
    },
}

impl Entry {
    fn event(e: DecodedEvent) -> Self {
        Entry::Event(Box::new((Stored::new(e), SigMemo::default())))
    }

    fn id(&self) -> EventId {
        match self {
            Entry::Event(b) => b.0.id,
            Entry::Link(l) => l.id,
            Entry::Floor { id, .. } => *id,
        }
    }

    fn parents(&self) -> &[EventId] {
        match self {
            Entry::Event(b) => &b.0.header.parents,
            Entry::Link(l) => &l.parents,
            Entry::Floor { .. } => &[],
        }
    }

    fn lamport(&self) -> u64 {
        match self {
            Entry::Event(b) => b.0.header.lamport,
            Entry::Link(l) => l.lamport,
            Entry::Floor { lamport, .. } => *lamport,
        }
    }

    /// 해소에 들어가는 항목인지(경계 아래 기록이 아님).
    fn is_live(&self) -> bool {
        !matches!(self, Entry::Floor { .. })
    }

    /// 본문을 아는 체크포인트 이벤트인지.
    fn is_checkpoint(&self) -> bool {
        matches!(self, Entry::Event(b) if b.0.header.kind == EventKind::StateCheckpoint)
    }

    fn item(&self, entry: usize) -> Item<'_> {
        match self {
            Entry::Event(b) => Item {
                id: b.0.id,
                parents: &b.0.header.parents,
                lamport: b.0.header.lamport,
                event: Some((&b.0, &b.1)),
                entry,
            },
            Entry::Link(l) => Item {
                id: l.id,
                parents: &l.parents,
                lamport: l.lamport,
                event: None,
                entry,
            },
            Entry::Floor { id, lamport } => Item {
                id: *id,
                parents: &[],
                lamport: *lamport,
                event: None,
                entry,
            },
        }
    }
}

/// 처리 기록: 지난 해소들이 항목을 처리한 순서(이어 붙인 것)와 그 사이의 스냅숏·바탕. 이 순서는 지금 항목 전부를
/// 창의 바탕부터 해소했을 때의 한 가지 올바른 처리 순서입니다(순서 연결끼리의 순서만 다를 수 있고, 이벤트 순서는 같음).
#[derive(Clone, Debug, Default)]
struct Log {
    /// 처리한 순서(`entries` 번호). 거부된 이벤트와 순서 연결도 들어갑니다(경계 아래 기록은 빠짐).
    order: Vec<usize>,
    /// `entries` 번호 → `order` 안의 위치(처리하지 않았으면 [`NOT_DONE`]).
    pos: Vec<usize>,
    /// 위치 오름차순. 마지막 바탕 뒤의 것만 있습니다.
    snapshots: Vec<Snapshot>,
    /// 받아들인 체크포인트(위치 오름차순).
    bases: Vec<BaseSlot>,
    /// 기록으로 재생할 수 없음: 기록 도중 시작 이벤트 고정이 정해졌거나(앞부분은 고정 없이 처리됨), 처리하지 못한
    /// 항목(순환)이 남음. 다음 재해소는 처음부터 하고 기록을 새로 만듭니다.
    broken: bool,
}

/// 한 번의 해소가 쓰는 항목 표와 설정. 체크포인트 경계를 처리합니다.
///
/// 경계 규칙(protocol.md "체크포인트"): 바탕 B 뒤의 항목 가운데 B를 잇는(앞 체크포인트 = B) 후보 C를 (논리 시계, ID)
/// 순으로 보고, C의 조상 가운데 B 뒤의 것(A)만 B 뒤 상태에서 해소한 뒤 C를 확인합니다. 맞으면 C가 새 바탕이 되고, A가
/// 아닌 나머지는 모두 C 뒤에 놓입니다. C의 확인은 A와 B 뒤 상태만의 함수라 어느 노드에서나 같습니다.
struct Engine<'a> {
    entries: &'a [Entry],
    index: &'a HashMap<EventId, usize>,
    ctx: Ctx,
}

/// 같은 바탕을 잇는 맞는 후보를 미리 보는 수 상한(소유자 기기가 일부러 갈래를 많이 내도 비용이 묶이게).
const MAX_FORKS: usize = 4;

/// 판정을 통과한 체크포인트 후보와 그 조상(A)까지 처리한 결과(시작점에서 떼어 둔 것).
struct Fork {
    c: usize,
    a: Vec<usize>,
    machine: Machine,
    out: Outcome,
    order: Vec<usize>,
    snapshots: Vec<Snapshot>,
    bases: Vec<BaseSlot>,
}

impl Engine<'_> {
    fn lamport(&self, id: &EventId) -> Option<u64> {
        self.index.get(id).map(|&i| self.entries[i].lamport())
    }

    /// 경계 판정 없이 위상 정렬로 처리합니다.
    fn run_plain(
        &self,
        idx: &[usize],
        machine: &mut Option<Machine>,
        judged: &HashMap<EventId, AuthError>,
        out: &mut Outcome,
        rec: Option<&mut Rec>,
    ) {
        let items: Vec<Item<'_>> = idx.iter().map(|&i| self.entries[i].item(i)).collect();
        let dag = EntriesDag {
            entries: self.entries,
            index: self.index,
        };
        run(&items, &dag, machine, self.ctx, judged, out, rec);
    }

    /// `idx`를 `machine`에서 이어 처리합니다(경계 포함).
    fn segment(
        &self,
        idx: Vec<usize>,
        machine: &mut Option<Machine>,
        out: &mut Outcome,
        rec: &mut Rec,
    ) {
        self.segment_with(idx, machine, out, rec, true);
    }

    /// [`Engine::segment`]. `lookahead`면 같은 바탕을 잇는 맞는 후보가 둘 이상일 때 그 뒤로 받아들여지는 체크포인트가
    /// 가장 많은 것을 고릅니다(같으면 (논리 시계, ID) 순으로 앞선 것). 더 깊은 갈림은 미리 보기 안에서 앞선 것으로
    /// 정합니다(미리 보기 비용을 갈림마다 [`MAX_FORKS`]배로 묶음).
    ///
    /// 왜 이어진 사슬을 보나: 이미 뒤 체크포인트를 받아 바탕을 옮기고 앞 구간을 버린 노드와 그 체크포인트로 들어온 멤버는
    /// 늦게 온 겨루는 체크포인트를 다시 판정할 수 없습니다. 그 노드들이 고른 쪽(뒤가 이어진 쪽)을 모든 노드가 고르게
    /// 합니다. (논리 시계, ID)만 보면 늦게 온 쪽이 이기는 노드와 판정할 수 없는 노드로 영영 갈립니다.
    fn segment_with(
        &self,
        mut idx: Vec<usize>,
        machine: &mut Option<Machine>,
        out: &mut Outcome,
        rec: &mut Rec,
        lookahead: bool,
    ) {
        loop {
            let floor = machine
                .as_ref()
                .and_then(|m| m.base.as_ref())
                .map_or(0, |b| b.floor);
            let (mut below, rest): (Vec<usize>, Vec<usize>) = idx
                .into_iter()
                .partition(|&i| self.entries[i].lamport() < floor);
            // 경계 아래 항목은 효과가 없습니다(체크포인트로 들어온 멤버는 이것을 모름). 처리했다고만 적습니다.
            below.sort_by_key(|&i| (self.entries[i].lamport(), self.entries[i].id()));
            for i in below {
                if matches!(self.entries[i], Entry::Event(_)) {
                    out.rejected
                        .push((self.entries[i].id(), AuthError::LateEvent));
                }
                rec.note(i, machine, out);
            }
            let candidates = self.candidates(&rest, machine);
            let mut judged = HashMap::new();
            let mut forks: Vec<Fork> = Vec::new();
            if !candidates.is_empty() {
                let set: HashSet<usize> = rest.iter().copied().collect();
                let lens = out.lens();
                let order_at = rec.order.len();
                let snapshots = rec.snapshots.clone();
                let bases_at = rec.bases.len();
                for c in candidates {
                    match self.try_candidate(c, &set, machine, &judged, out, rec) {
                        Ok((a, m)) => {
                            // 조상까지 처리한 결과를 떼어 두고 시작점으로 되돌립니다(다른 후보도 같은 상태에서 판정).
                            forks.push(Fork {
                                c,
                                a,
                                machine: m,
                                out: out.take_tail(&lens),
                                order: rec.order.split_off(order_at),
                                snapshots: std::mem::replace(&mut rec.snapshots, snapshots.clone()),
                                bases: rec.bases.split_off(bases_at),
                            });
                            if !lookahead || forks.len() >= MAX_FORKS {
                                break;
                            }
                        }
                        Err(e) => {
                            judged.insert(self.entries[c].id(), e);
                        }
                    }
                }
            }
            let chosen = match forks.len() {
                0 => None,
                1 => forks.pop(),
                _ => {
                    let mut best: Option<(usize, usize)> = None;
                    for (k, f) in forks.iter().enumerate() {
                        let (mut m, mut o, mut r) = (machine.clone(), out.clone(), rec.clone());
                        let remaining = self.adopt(f, &rest, &mut m, &mut o, &mut r);
                        let before = o.checkpoints.len();
                        self.segment_with(remaining, &mut m, &mut o, &mut r, false);
                        let score = o.checkpoints.len() - before;
                        if best.is_none_or(|(_, s)| score > s) {
                            best = Some((k, score));
                        }
                    }
                    best.map(|(k, _)| forks.swap_remove(k))
                }
            };
            let Some(f) = chosen else {
                self.run_plain(&rest, machine, &judged, out, Some(rec));
                return;
            };
            idx = self.adopt(&f, &rest, machine, out, rec);
        }
    }

    /// 판정한 후보 `f`를 받아들입니다: 조상까지 처리한 결과를 붙이고 체크포인트를 바탕으로 삼습니다. 남은 항목을 돌려줍니다.
    fn adopt(
        &self,
        f: &Fork,
        rest: &[usize],
        machine: &mut Option<Machine>,
        out: &mut Outcome,
        rec: &mut Rec,
    ) -> Vec<usize> {
        out.append(&f.out);
        rec.order.extend_from_slice(&f.order);
        rec.snapshots = f.snapshots.clone();
        rec.bases.extend(f.bases.iter().cloned());
        let cid = self.entries[f.c].id();
        out.applied.push(cid);
        out.checkpoints.push(cid);
        out.changed = true;
        *machine = Some(f.machine.clone());
        rec.note(f.c, machine, out);
        if let Some(m) = machine.as_ref() {
            rec.accept(f.c, m, out);
        }
        let used: HashSet<usize> = f.a.iter().copied().chain([f.c]).collect();
        rest.iter().copied().filter(|i| !used.contains(i)).collect()
    }

    /// 바탕을 이을 수 있는 체크포인트 후보((논리 시계, ID) 순). 바탕 정보와 체크포인트만으로 볼 수 있는 것만 봅니다: 판,
    /// 앞 체크포인트와 순번, 상태 키, 헤더와 본문의 일치, 서명. 지금 상태(증분이면 바탕 뒤 일부를 처리한 상태)를 보면
    /// 증분 해소와 다시 해소의 후보가 달라질 수 있습니다.
    fn candidates(&self, idx: &[usize], machine: &Option<Machine>) -> Vec<usize> {
        let base = machine.as_ref().and_then(|m| m.base.as_ref());
        let mut out: Vec<usize> = idx
            .iter()
            .copied()
            .filter(|&i| {
                let Entry::Event(b) = &self.entries[i] else {
                    return false;
                };
                let (ev, memo) = (&b.0, &b.1);
                let Some(c) = checkpoint_of(ev) else {
                    return false;
                };
                let body = &c.body;
                if body.resolver_version > RESOLVER_VERSION {
                    return false;
                }
                let next = match base {
                    Some(b) => body.prev == Some(b.id) && body.seq == b.seq + 1,
                    None => body.prev.is_none() && body.seq == 1,
                };
                let key_ok = matches!(ev.header.key, KeyRef::State(k) if body.valid_state_keys.binary_search(&k).is_ok());
                next && key_ok
                    && ev.header.space == body.space
                    && ev.header.author == body.author
                    && memo.checkpoint_ok(c)
            })
            .collect();
        out.sort_by_key(|&i| (self.entries[i].lamport(), self.entries[i].id()));
        out
    }

    /// 후보 `c`를 판정합니다: 부모·경계를 보고, 그 조상(A, `set` 안)을 지금 상태에서 해소한 뒤 상태를 확인합니다. 맞으면
    /// A(처리 기록에 남김)와 C를 바탕으로 삼은 상태를, 아니면 이유를 돌려주고 기록을 되돌립니다.
    fn try_candidate(
        &self,
        c: usize,
        set: &HashSet<usize>,
        machine: &Option<Machine>,
        judged: &HashMap<EventId, AuthError>,
        out: &mut Outcome,
        rec: &mut Rec,
    ) -> Result<(Vec<usize>, Machine), AuthError> {
        let Entry::Event(b) = &self.entries[c] else {
            return Err(AuthError::InvalidCheckpoint("본문 없음"));
        };
        let ev = &b.0;
        let cp = checkpoint_of(ev).ok_or(AuthError::KindMismatch)?;
        let base = machine.as_ref().and_then(|m| m.base.as_ref());
        let mut ps = ev.header.parents.clone();
        ps.sort_unstable();
        ps.dedup();
        if ps != cp.body.heads {
            return Err(AuthError::InvalidCheckpoint("부모"));
        }
        let mut maxlam = 0;
        for p in &ps {
            let l = self
                .lamport(p)
                .ok_or(AuthError::InvalidCheckpoint("모르는 부모"))?;
            maxlam = maxlam.max(l);
        }
        // A: C의 조상 가운데 이번 구간의 것. 바탕 B를 이어야 합니다.
        let mut reached = base.is_none();
        let mut a = Vec::new();
        let mut seen = HashSet::new();
        let mut stack: Vec<EventId> = ps.clone();
        while let Some(id) = stack.pop() {
            let Some(&j) = self.index.get(&id) else {
                continue;
            };
            if set.contains(&j) {
                if seen.insert(j) {
                    a.push(j);
                    stack.extend_from_slice(self.entries[j].parents());
                }
            } else if base.is_some_and(|b| b.event == id) {
                reached = true;
            }
        }
        if !reached {
            return Err(AuthError::InvalidCheckpoint("앞 체크포인트를 잇지 않음"));
        }
        let f = cp.body.frontier_lamport;
        let f_min = base
            .map_or(0, |b| b.floor)
            .max(maxlam.saturating_sub(FRONTIER_DEPTH));
        if f < f_min || f > maxlam {
            return Err(AuthError::InvalidCheckpoint("경계"));
        }
        if frontier_of(self.entries, a.iter().copied(), base, f) != cp.body.frontier {
            return Err(AuthError::InvalidCheckpoint("경계 위 조상"));
        }
        let lens = out.lens();
        let saved_order = rec.order.len();
        let saved_snapshots = rec.snapshots.clone();
        let saved_bases = rec.bases.len();
        let mut m = machine.clone();
        self.run_plain(&a, &mut m, judged, out, Some(rec));
        let key = match ev.header.key {
            KeyRef::State(k) => Some(k),
            _ => None,
        };
        let result = match &m {
            Some(m) => m.accept_checkpoint(ev, cp, key),
            None => Err(AuthError::NoSpace),
        };
        match result {
            Ok(next) => Ok((a, next)),
            Err(e) => {
                out.restore(&lens);
                rec.order.truncate(saved_order);
                rec.snapshots = saved_snapshots;
                rec.bases.truncate(saved_bases);
                Err(e)
            }
        }
    }
}

/// 이벤트가 체크포인트면 그 본문.
fn checkpoint_of(ev: &Stored) -> Option<&SignedCheckpoint> {
    match &ev.body {
        StoredBody::Full(b) => match &**b {
            EventBody::StateCheckpoint(c) => Some(c),
            _ => None,
        },
        _ => None,
    }
}

/// 경계 위 조상: `ancestors`(바탕 뒤 조상)와 바탕·바탕의 경계 위 조상 가운데 논리 시계가 `f` 이상인 것(ID 순).
fn frontier_of(
    entries: &[Entry],
    ancestors: impl Iterator<Item = usize>,
    base: Option<&Arc<CheckpointBase>>,
    f: u64,
) -> Vec<FrontierEntry> {
    let mut map: BTreeMap<EventId, u64> = BTreeMap::new();
    for j in ancestors {
        let l = entries[j].lamport();
        if l >= f {
            map.insert(entries[j].id(), l);
        }
    }
    if let Some(b) = base {
        if b.lamport >= f {
            map.insert(b.event, b.lamport);
        }
        for e in b.frontier.iter() {
            if e.lamport >= f {
                map.insert(e.id, e.lamport);
            }
        }
    }
    map.into_iter()
        .map(|(id, lamport)| FrontierEntry { id, lamport })
        .collect()
}

/// 체크포인트를 만들 때 해소기가 채우는 값([`Resolver::checkpoint_draft`]). 데몬이 스냅숏을 암호화해 블롭 ID·크기를
/// 넣고, 소유자 기기 목록·쓴 기기·위임을 더해 서명합니다.
#[derive(Clone, Debug)]
pub struct CheckpointDraft {
    pub seq: u64,
    pub prev: Option<CheckpointId>,
    pub heads: Vec<EventId>,
    /// 체크포인트 이벤트에 쓸 논리 시계(부모 최대 + 1).
    pub lamport: u64,
    pub frontier_lamport: u64,
    pub frontier: Vec<FrontierEntry>,
    pub state_hash: [u8; 32],
    pub user_root: [u8; 32],
    pub users: u64,
    pub device_root: [u8; 32],
    pub devices: u64,
    pub key_heads: Vec<KeyHead>,
    pub valid_state_keys: Vec<StateKeyId>,
    pub owner: UserId,
    /// 지금 상태의 소유자 기기 기록(본문에 실을 서명된 목록이 이것과 같아야 함).
    pub owner_record: Arc<MemberDevices>,
    pub owner_proof: MerkleProof,
    pub revoked: Vec<DeviceId>,
    pub revocation_root: [u8; 32],
    pub owner_chain: Vec<OwnershipCert>,
    /// 다음 체크포인트를 쓸 수 있는 위임의 해시(본문의 `delegation_hash`).
    pub delegation_hash: Option<[u8; 32]>,
    /// 소유자가 바뀐 구간이면 바탕 기기 트리의 잎 증명 후보: 두 번째 이후 인증서의 기기와 새 소유자의 활성 기기 전부.
    /// 본문에는 [`CheckpointDraft::signer_proofs_for`]로 쓴 기기 것만 고릅니다.
    pub signer_proofs: Vec<DeviceLeafProof>,
    /// 스냅숏 평문(정규 인코딩).
    pub snapshot: Vec<u8>,
}

impl CheckpointDraft {
    /// 쓴 기기가 아는 비공개·덮개 그룹의 이긴 사슬 끝을 키 머리에 더합니다(그룹 순, 크루 머리는 해소기 것을 그대로,
    /// 순번 0과 상한을 넘는 것은 뺌). 해소기는 이 머리를 확인하지 않고, 새로 자격을 얻은 기기가 그룹 사슬을 여기서
    /// 시작합니다.
    pub fn add_group_heads(&mut self, space: &SpaceId, groups: impl IntoIterator<Item = KeyHead>) {
        let crew = yangpa_core::ids::KeyGroupId::crew(space);
        let mut all: BTreeMap<yangpa_core::ids::KeyGroupId, KeyHead> =
            self.key_heads.iter().map(|k| (k.group, *k)).collect();
        for k in groups {
            if k.group != crew && k.seq > 0 {
                all.entry(k.group).or_insert(k);
            }
        }
        let crew_head = all.remove(&crew);
        let mut out: Vec<KeyHead> = crew_head.into_iter().collect();
        out.extend(
            all.into_values()
                .take(yangpa_core::checkpoint::MAX_KEY_HEADS - out.len()),
        );
        out.sort_by_key(|k| k.group);
        self.key_heads = out;
    }

    /// 기기 `author`가 쓸 때 본문에 실을 서명 기기 증명(소유자가 바뀌지 않았으면 비어 있음).
    pub fn signer_proofs_for(&self, author: &DeviceId) -> Vec<DeviceLeafProof> {
        let Some(last) = self.owner_chain.last() else {
            return Vec::new();
        };
        let mut want: Vec<(DeviceId, UserId)> = self.owner_chain[1..]
            .iter()
            .map(|c| (c.device, c.from))
            .collect();
        want.push((*author, last.to));
        want.sort();
        want.dedup();
        want.iter()
            .filter_map(|(d, u)| {
                self.signer_proofs
                    .iter()
                    .find(|p| p.record.device == *d && p.user == *u)
                    .cloned()
            })
            .collect()
    }
}

/// 체크포인트로 해소기를 만들 수 없는 이유.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum CheckpointError {
    #[error("체크포인트를 쓸 수 없음: {0}")]
    Invalid(&'static str),
    #[error("더 새 해소기 판의 체크포인트(업데이트 필요)")]
    NewerResolver,
}

/// 증분 상태 해소기.
///
/// 새로 온 이벤트가 이미 처리한 모든 이벤트보다 '확실히 뒤'이면(지금의 모든 머리를 조상으로 두고,
/// 기존 이벤트의 빠진 부모를 채우지 않으면) 이어서 적용만 합니다. 이 경우 전체 재해소의 순서에서도
/// 새 이벤트가 기존 이벤트 전부보다 뒤에 오므로 결과가 같습니다. 그렇지 않으면(동시 이벤트, 빈 구간 채움, 본문을
/// 받은 순서 연결) 처리 기록에서 새 이벤트가 영향을 줄 수 있는 가장 이른 위치를 찾아, 그 앞의 스냅숏에서 나머지만
/// 다시 해소합니다([`Resolver::insert`]). 결과(적용·거부와 그 순서, 상태, 알리는 차이)는 처음부터 다시 해소한
/// 것([`Resolver::rebuild`])과 같습니다.
///
/// 체크포인트(protocol.md "체크포인트"): 받아들인 체크포인트가 경계(바탕)입니다. 바탕이 둘 쌓이면 앞 바탕까지의 항목을
/// RAM에서 버리고(논리 시계만 남김) 그 상태를 창의 바탕으로 삼습니다. 그래서 RAM과 다시 해소하는 범위는 마지막 두
/// 체크포인트 사이와 그 뒤의 이벤트로 묶입니다. 바탕 앞을 건드리는 변화(겨루는 체크포인트 등)는 창의 바탕부터 다시
/// 해소합니다. 체크포인트로 들어온 멤버는 [`Resolver::from_checkpoint`]로 시작합니다.
///
/// 시작 이벤트 고정: [`Resolver::with_genesis`]로 고정하지 않았더라도, 공간 생성 이벤트를 처음 적용하는
/// 순간 그 이벤트로 고정합니다(처음 본 것을 믿음). 그래서 나중에 멤버가 더 작은 논리 시계로 만든 가짜
/// 공간 생성 이벤트가 와서 재해소가 일어나도 소유자가 바뀌지 않습니다. 재시작 뒤에도 같게 하려면 데몬이
/// [`Resolver::genesis`]를 저장해 두고 [`Resolver::with_genesis`]로 만들어야 합니다.
///
/// 본문을 아직 풀지 못한 상태 이벤트는 [`LinkEvent`]로 먼저 넣어 두었다가, 풀린 뒤 같은 ID의
/// [`DecodedEvent`]로 다시 넣으면 적용 대상으로 바뀝니다(그 순서 연결을 처리한 위치부터 다시 해소).
#[derive(Clone, Debug, Default)]
pub struct Resolver {
    pin: Option<EventId>,
    limits: Limits,
    /// 받은 순서의 항목.
    entries: Vec<Entry>,
    /// ID → `entries` 번호.
    index: HashMap<EventId, usize>,
    /// 누군가 부모로 가리킨 ID(알든 모르든).
    referenced: HashSet<EventId>,
    /// 아직 받지 않은 부모 ID → 그것을 부모로 둔 항목(`entries` 번호). 빈 구간이 채워질 때 다시 해소할 위치를 찾습니다.
    waiting: HashMap<EventId, Vec<usize>>,
    /// 아무도 부모로 가리키지 않은 알려진 항목(경계 아래 항목은 빠짐).
    heads: BTreeSet<EventId>,
    machine: Option<Machine>,
    /// 창 안의 결과(창의 바탕 앞 것은 버림).
    applied: Vec<EventId>,
    rejected: Vec<(EventId, AuthError)>,
    key_events: Vec<EventId>,
    log: Log,
    stats: ResolverStats,
    /// 창의 바탕: 처리 기록이 시작하는 체크포인트와 그 뒤 상태. 없으면 처음(공간 생성 전)부터.
    window: Option<Box<BaseSlot>>,
    /// 이미 알린 겨룬 체크포인트.
    reported: HashSet<EventId>,
    /// 서명이 맞는 체크포인트 이벤트(순번 → (이벤트, 쓴 기기)). 받아들인 바탕과 같은 순번의 다른 것을 겨룬 것으로 알립니다
    /// (경계 아래로 와서 본문을 버린 것도). 최근 [`KEEP_SEEN_SEQS`] 순번만.
    seen_checkpoints: BTreeMap<u64, Vec<(EventId, DeviceId)>>,
    /// 받아들인 바탕(순번 → (이벤트, 쓴 기기)). 창에서 버린 바탕도 최근 [`KEEP_SEEN_SEQS`] 순번까지 기억합니다.
    accepted_bases: BTreeMap<u64, (EventId, DeviceId)>,
    /// 이미 아는 이벤트의 조상 집합이 바뀐 횟수(빠진 부모가 오거나 순서 연결로만 알던 상태 이벤트의 본문이 옴).
    /// 빈 구간을 낀 기준점으로 한 판정([`Resolver::point_at`])은 이것이 바뀌면 다시 합니다.
    gap_epoch: u64,
}

/// 겨룬 체크포인트를 견주려고 기억하는 순번 폭.
const KEEP_SEEN_SEQS: u64 = 8;

/// 머리가 이보다 많으면 덮음 검사 대신 전체 재해소합니다.
const MAX_COVER_HEADS: usize = 1024;

impl Resolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// 시작 이벤트(공간 생성)를 고정합니다. [`resolve_with`]의 `genesis`와 같습니다.
    pub fn with_genesis(genesis: EventId) -> Self {
        Self::with_genesis_limits(genesis, Limits::default())
    }

    /// [`Resolver::with_genesis`]에 상태 한도를 바꾼 것(규모 시뮬레이션용).
    pub fn with_genesis_limits(genesis: EventId, limits: Limits) -> Self {
        Resolver {
            pin: Some(genesis),
            limits,
            ..Self::default()
        }
    }

    /// 체크포인트로 시작합니다(새 멤버·재시작). `checkpoint`는 본문을 푼 체크포인트 이벤트, `snapshot`은 스냅숏 평문
    /// (정규 인코딩)입니다. 서명·상태 해시·트리 뿌리·소유자 기록이 맞는지 봅니다. 체크포인트 사슬이 초대 링크의 고정에서
    /// 이어지는지는 부르는 쪽이 [`crate::checkpoint::validate_chain`]으로 봅니다.
    pub fn from_checkpoint(
        genesis: EventId,
        checkpoint: DecodedEvent,
        snapshot: &[u8],
    ) -> Result<Resolver, CheckpointError> {
        Self::from_checkpoint_limits(genesis, checkpoint, snapshot, Limits::default())
    }

    /// [`Resolver::from_checkpoint`]에 상태 한도를 바꾼 것(규모 시뮬레이션용).
    pub fn from_checkpoint_limits(
        genesis: EventId,
        checkpoint: DecodedEvent,
        snapshot: &[u8],
        limits: Limits,
    ) -> Result<Resolver, CheckpointError> {
        let DecodedEvent { id, header, body } = checkpoint;
        let EventBody::StateCheckpoint(c) = body else {
            return Err(CheckpointError::Invalid("체크포인트 이벤트가 아님"));
        };
        if header.kind != EventKind::StateCheckpoint {
            return Err(CheckpointError::Invalid("헤더 종류"));
        }
        if c.body.resolver_version > RESOLVER_VERSION {
            return Err(CheckpointError::NewerResolver);
        }
        let key = match header.key {
            KeyRef::State(k) if c.body.valid_state_keys.binary_search(&k).is_ok() => k,
            _ => return Err(CheckpointError::Invalid("상태 키")),
        };
        let mut ps = header.parents.clone();
        ps.sort_unstable();
        ps.dedup();
        if header.space != c.body.space || header.author != c.body.author || ps != c.body.heads {
            return Err(CheckpointError::Invalid("헤더와 본문이 다름"));
        }
        if c.verify().is_err() {
            return Err(CheckpointError::Invalid("서명"));
        }
        let mut machine = Machine::decode(snapshot, limits).map_err(CheckpointError::Invalid)?;
        if machine.genesis != genesis {
            return Err(CheckpointError::Invalid("시작 이벤트"));
        }
        machine
            .check_snapshot(id, header.lamport, Some(key), &c)
            .map_err(CheckpointError::Invalid)?;
        let mut r = Resolver {
            pin: Some(genesis),
            limits,
            ..Self::default()
        };
        r.entries.push(Entry::Floor {
            id,
            lamport: header.lamport,
        });
        for f in &c.body.frontier {
            if f.id != id {
                r.entries.push(Entry::Floor {
                    id: f.id,
                    lamport: f.lamport,
                });
                r.referenced.insert(f.id);
            }
        }
        for (i, e) in r.entries.iter().enumerate() {
            r.index.insert(e.id(), i);
        }
        r.log.pos = vec![NOT_DONE; r.entries.len()];
        r.heads.insert(id);
        r.applied.push(id);
        r.window = Some(Box::new(BaseSlot {
            mark: Mark::default(),
            entry: 0,
            machine: machine.clone(),
        }));
        r.machine = Some(machine);
        r.note_bases();
        Ok(r)
    }

    /// 상태·키 합의 이벤트를 넣습니다(키 레인 이벤트는 순서 연결로 씀).
    pub fn insert_events(&mut self, events: Vec<DecodedEvent>) -> ResolveUpdate {
        self.insert(events, Vec::new())
    }

    /// 창의 바탕 경계(그 아래로 오는 항목은 처리하지 않음).
    fn window_floor(&self) -> u64 {
        self.window
            .as_ref()
            .and_then(|w| w.machine.base.as_ref())
            .map_or(0, |b| b.floor)
    }

    /// 지금 바탕의 경계 F(바탕이 없으면 0). 데몬은 새 이벤트의 부모에서 이 아래 머리를 뺍니다(늦은 이벤트가 되지 않게).
    pub fn floor(&self) -> u64 {
        self.machine
            .as_ref()
            .and_then(|m| m.base.as_ref())
            .map_or(0, |b| b.floor)
    }

    /// 이벤트와 순서 연결용 이벤트를 넣고 상태를 갱신합니다. 이미 아는 ID는 무시하되, 순서 연결로만
    /// 알던 ID가 본문과 함께 오면 적용 대상으로 바꿉니다. 창의 바탕 경계 아래로 온 것은 논리 시계만 남깁니다.
    ///
    /// 새 항목이 기존 전부보다 확실히 뒤이면 이어서 처리하고, 아니면 처리 기록의 스냅숏에서 다시 해소합니다
    /// ([`Resolver::replay_start`]). 기록을 쓸 수 없으면 처음부터 다시 해소합니다.
    pub fn insert(&mut self, events: Vec<DecodedEvent>, links: Vec<LinkEvent>) -> ResolveUpdate {
        let wfloor = self.window_floor();
        let mut batch_ids = HashSet::new();
        let mut new_events = Vec::new();
        let mut new_links = Vec::new();
        let mut floors: Vec<(EventId, u64, bool)> = Vec::new();
        let mut upgraded = Vec::new();
        // 체크포인트가 들면 증분으로 처리하지 않고(후보의 조상을 실제로 따라가 보게), 지금 바탕을 바로 잇지 않는 것(겨루는
        // 것, 진 쪽을 잇는 것)이면 창의 바탕부터 다시 판정합니다.
        let mut checkpoint = false;
        let mut off_chain = false;
        for e in events {
            if is_scoped(&e.header) {
                continue;
            }
            let link_only = is_link_only(&e);
            if let Some(&i) = self.index.get(&e.id) {
                // 같은 ID는 같은 헤더(해시)이므로 DAG 기록은 그대로 두고 본문만 붙입니다.
                if !link_only && matches!(self.entries[i], Entry::Link(_)) && batch_ids.insert(e.id)
                {
                    let (c, off) = self.note_checkpoint(&e);
                    checkpoint |= c;
                    off_chain |= off;
                    upgraded.push((i, e));
                }
                continue;
            }
            if !batch_ids.insert(e.id) {
                continue;
            }
            let (c, off) = self.note_checkpoint(&e);
            checkpoint |= c;
            off_chain |= off;
            if e.header.lamport < wfloor {
                floors.push((e.id, e.header.lamport, !link_only));
            } else if link_only {
                new_links.push(LinkEvent::from_header(e.id, &e.header));
            } else {
                new_events.push(e);
            }
        }
        for l in links {
            if self.index.contains_key(&l.id) || !batch_ids.insert(l.id) {
                continue;
            }
            if l.lamport < wfloor {
                floors.push((l.id, l.lamport, false));
            } else {
                new_links.push(l);
            }
        }
        if batch_ids.is_empty() {
            return ResolveUpdate::default();
        }
        let below: Vec<(EventId, AuthError)> = floors
            .iter()
            .filter(|f| f.2)
            .map(|f| (f.0, AuthError::BelowCheckpoint))
            .collect();
        let before = self.base_events();

        // 본문을 받은 순서 연결은 이미 순서에 들어 있던 자리에 효과가 생기므로 증분으로는 맞출 수 없습니다.
        let fills_gap = batch_ids.iter().any(|id| self.waiting.contains_key(id));
        if fills_gap || !upgraded.is_empty() || checkpoint {
            self.gap_epoch += 1;
        }
        let live_new = !new_events.is_empty() || !new_links.is_empty();
        let incremental = upgraded.is_empty()
            && !fills_gap
            && !checkpoint
            && self.covers_heads(&new_events, &new_links);
        // 빠진 부모 기록(`waiting`)이 새 항목 등록으로 지워지기 전에 셉니다.
        let replay_at = (!incremental && !off_chain && !self.log.broken && replay_enabled())
            .then(|| self.replay_start(&new_events, &new_links, &floors, &upgraded));

        for (i, e) in upgraded {
            // 순서 연결로 이미 부모 참조를 셌습니다(같은 ID는 같은 부모).
            self.entries[i] = Entry::event(e);
        }
        let start = self.entries.len();
        self.entries
            .extend(new_events.into_iter().map(Entry::event));
        self.entries.extend(new_links.into_iter().map(Entry::Link));
        self.entries.extend(
            floors
                .iter()
                .map(|&(id, lamport, _)| Entry::Floor { id, lamport }),
        );
        self.register(start);

        let mut update = if incremental {
            if live_new {
                self.extend(start)
            } else {
                ResolveUpdate::default()
            }
        } else {
            match replay_at {
                Some(p) => self.replay(p, start),
                None => self.rebuild(),
            }
        };
        update.rejected.extend(below);
        self.after_resolution(&before, &mut update);
        update
    }

    /// 넣는 이벤트가 체크포인트면 기억하고, (체크포인트인지, 지금 바탕을 바로 잇지 않는지)를 돌려줍니다. 서명이 맞는 것만
    /// 기억하고 다시 판정하게 합니다(소유자 신원 없이 아무 멤버나 비싼 재해소를 일으키지 못하게).
    fn note_checkpoint(&mut self, e: &DecodedEvent) -> (bool, bool) {
        if e.header.kind != EventKind::StateCheckpoint {
            return (false, false);
        }
        let EventBody::StateCheckpoint(c) = &e.body else {
            return (true, false);
        };
        if c.body.space != e.header.space
            || c.body.author != e.header.author
            || c.body.resolver_version > RESOLVER_VERSION
            || c.verify().is_err()
        {
            return (true, false);
        }
        let seq = c.body.seq;
        let list = self.seen_checkpoints.entry(seq).or_default();
        if !list.iter().any(|(id, _)| *id == e.id) {
            list.push((e.id, c.body.author));
        }
        let off = match self.machine.as_ref().and_then(|m| m.base.as_ref()) {
            Some(b) => seq <= b.seq || (seq == b.seq + 1 && c.body.prev != Some(b.id)),
            None => false,
        };
        (true, off)
    }

    /// 항목 `i`가 서명이 맞아 기억한 체크포인트이면서 지금 바탕 사슬에 들지 않고 바탕을 바로 잇지도 않는지
    /// ([`Resolver::note_checkpoint`]와 같은 기준, 받아들인 바탕은 뺌).
    fn off_chain_entry(&self, i: usize) -> bool {
        let Entry::Event(b) = &self.entries[i] else {
            return false;
        };
        let StoredBody::Full(body) = &b.0.body else {
            return false;
        };
        let EventBody::StateCheckpoint(c) = body.as_ref() else {
            return false;
        };
        let (id, seq) = (b.0.id, c.body.seq);
        let seen = self
            .seen_checkpoints
            .get(&seq)
            .is_some_and(|l| l.iter().any(|(x, _)| *x == id));
        if !seen || self.accepted_bases.get(&seq).is_some_and(|(e, _)| *e == id) {
            return false;
        }
        match self.machine.as_ref().and_then(|m| m.base.as_ref()) {
            Some(base) => seq <= base.seq || (seq == base.seq + 1 && c.body.prev != Some(base.id)),
            None => false,
        }
    }

    /// 해소 뒤 정리: 새 바탕 알리기, 바탕이 바뀌었으면 머리 다시 세기, 겨룬 체크포인트 한 번만 알리기, 창 줄이기.
    fn after_resolution(&mut self, before: &[EventId], update: &mut ResolveUpdate) {
        let after = self.base_events();
        update.checkpoints = after
            .iter()
            .filter(|e| !before.contains(e))
            .copied()
            .collect();
        let changed = after != before;
        if self.compact() || changed {
            self.rebuild_dag();
        }
        self.note_bases();
        let mut conflicts = Vec::new();
        for (seq, (base, author)) in &self.accepted_bases {
            for (other, by) in self.seen_checkpoints.get(seq).into_iter().flatten() {
                if other != base && self.reported.insert(*other) {
                    conflicts.push(CheckpointConflict {
                        base: *base,
                        other: *other,
                        same_author: by == author,
                    });
                }
            }
        }
        update.conflicts = conflicts;
    }

    /// 받아들인 바탕을 순번으로 기억하고 오래된 기억을 버립니다.
    fn note_bases(&mut self) {
        let slots = self
            .window
            .iter()
            .map(|w| &w.machine)
            .chain(self.log.bases.iter().map(|b| &b.machine))
            .chain(self.machine.as_ref());
        for m in slots {
            if let Some(b) = &m.base {
                self.accepted_bases.insert(b.seq, (b.event, b.author));
            }
        }
        // 다시 판정해 바탕이 물러났으면(겨루는 쪽이 이김) 그 뒤 순번의 기억은 지금 결과가 아닙니다.
        let top = self
            .machine
            .as_ref()
            .and_then(|m| m.base.as_ref())
            .map_or(0, |b| b.seq);
        self.accepted_bases.retain(|s, _| *s <= top);
        let keep = top.saturating_sub(KEEP_SEEN_SEQS);
        self.accepted_bases.retain(|s, _| *s >= keep);
        self.seen_checkpoints.retain(|s, _| *s >= keep);
    }

    /// 창 안의 바탕 체크포인트 이벤트(차례대로).
    fn base_events(&self) -> Vec<EventId> {
        self.window
            .iter()
            .map(|w| w.entry)
            .chain(self.log.bases.iter().map(|b| b.entry))
            .map(|i| self.entries[i].id())
            .collect()
    }

    /// DAG 기록 갱신(`entries[start..]`가 새 항목). 먼저 전부 등록한 뒤 부모 참조를 반영해야 묶음 안 참조도 머리에서
    /// 빠집니다.
    fn register(&mut self, start: usize) {
        let floor = self.floor();
        for i in start..self.entries.len() {
            let id = self.entries[i].id();
            self.index.insert(id, i);
            self.waiting.remove(&id);
            if !self.referenced.contains(&id) && self.counts(i, floor) {
                self.heads.insert(id);
            }
        }
        for i in start..self.entries.len() {
            self.refer(i, floor);
        }
        self.log.pos.resize(self.entries.len(), NOT_DONE);
    }

    /// 머리·빠진 부모 계산에 드는 항목인지: 경계 F 이상의 해소 항목. F 아래 항목(늦은 이벤트, 지어낸 낮은 논리 시계)은
    /// 효과가 없으므로 그 부모 참조를 세지 않습니다. 세면 F 위의 진짜 머리가 숨어, 증분 해소가 처음부터와 달라지고
    /// 체크포인트를 영영 만들 수 없습니다.
    fn counts(&self, i: usize, floor: u64) -> bool {
        self.entries[i].is_live() && self.entries[i].lamport() >= floor
    }

    /// 항목 `i`의 부모 참조를 기록합니다.
    fn refer(&mut self, i: usize, floor: u64) {
        if !self.counts(i, floor) {
            return;
        }
        for p in self.entries[i].parents() {
            self.heads.remove(p);
            self.referenced.insert(*p);
            if !self.index.contains_key(p) {
                self.waiting.entry(*p).or_default().push(i);
            }
        }
    }

    /// 바탕(경계)이 바뀐 뒤 머리·참조·빠진 부모를 다시 셉니다([`Resolver::counts`]). 바탕 이벤트와 경계 위 조상은
    /// 경계 아래 기록이어도 아는 것으로 셉니다(바탕은 머리가 될 수 있고, 경계 위 조상은 바탕이 덮음).
    fn rebuild_dag(&mut self) {
        let floor = self.floor();
        let base = self.machine.as_ref().and_then(|m| m.base.clone());
        self.referenced.clear();
        self.waiting.clear();
        self.heads.clear();
        if let Some(b) = &base {
            self.referenced.extend(b.frontier.iter().map(|f| f.id));
        }
        for i in 0..self.entries.len() {
            if !self.counts(i, floor) {
                continue;
            }
            for p in self.entries[i].parents() {
                self.referenced.insert(*p);
                if !self.index.contains_key(p) {
                    self.waiting.entry(*p).or_default().push(i);
                }
            }
        }
        for i in 0..self.entries.len() {
            let id = self.entries[i].id();
            let is_base = base.as_ref().is_some_and(|b| b.event == id);
            if (is_base || self.counts(i, floor)) && !self.referenced.contains(&id) {
                self.heads.insert(id);
            }
        }
    }

    /// 처음 적용된 공간 생성 이벤트로 고정합니다(이미 고정했으면 그대로).
    fn pin_genesis(&mut self) {
        if self.pin.is_none() {
            self.pin = self.machine.as_ref().map(|m| m.genesis);
            // 고정이 생기면 그 앞의 처리 기록은 고정 없이 만든 것입니다. 고정은 공간 생성 이벤트의 판정만 바꾸므로(다른
            // 공간 생성 이벤트의 거부 이유), 고정한 것 말고 다른 공간 생성 이벤트를 처리했을 때만 다음 재해소를 처음부터
            // 합니다. 늘 깨진 것으로 두면 만든 노드는 증분 해소만 이어지는 동안 기록이 계속 깨져 체크포인트를 내지 못함.
            if let Some(g) = self.pin {
                let entries = &self.entries;
                let other_create = self.log.order.iter().any(|&i| match &entries[i] {
                    Entry::Event(b) => {
                        b.0.id != g
                            && (b.0.header.kind == EventKind::SpaceCreate
                                || matches!(&b.0.body, StoredBody::Full(x)
                                    if matches!(**x, EventBody::SpaceCreate { .. })))
                    }
                    _ => false,
                });
                self.log.broken |= other_create;
            }
        }
    }

    fn ctx(&self) -> Ctx {
        Ctx {
            pin: self.pin,
            limits: self.limits,
        }
    }

    /// 지금 처리 기록의 끝.
    fn mark(&self) -> Mark {
        Mark {
            at: self.log.order.len(),
            applied: self.applied.len(),
            rejected: self.rejected.len(),
            key_events: self.key_events.len(),
        }
    }

    /// 마지막 바탕의 위치(처리 기록에서). 바탕이 처리 기록에 없으면 0(창의 바탕이나 처음).
    fn segment_at(&self) -> usize {
        self.log.bases.last().map_or(0, |b| b.mark.at)
    }

    /// 마지막 구간의 시작 위치와 상태(바탕이 없고 창도 없으면 `None`: 처음부터).
    fn segment_start(&self) -> Option<(Mark, Machine)> {
        match self.log.bases.last() {
            Some(b) => Some((b.mark, b.machine.clone())),
            None => self
                .window
                .as_ref()
                .map(|w| (Mark::default(), w.machine.clone())),
        }
    }

    /// 실행 결과의 처리 순서와 스냅숏을 기록에 붙입니다.
    fn record(&mut self, rec: Rec, stuck: bool) {
        debug_assert_eq!(self.log.order.len(), rec.base.at);
        for e in rec.order {
            self.log.pos[e] = self.log.order.len();
            self.log.order.push(e);
        }
        self.log.snapshots = rec.snapshots;
        self.log.bases.extend(rec.bases);
        // 처리하지 못한 항목(순환)이 있으면 위치를 셀 수 없습니다.
        self.log.broken |= stuck;
    }

    /// 새 항목(`entries[start..]`)이 기존 전부보다 뒤일 때: 지금 상태에서 이어서 처리합니다.
    fn extend(&mut self, start: usize) -> ResolveUpdate {
        self.stats.incremental += 1;
        let idx: Vec<usize> = (start..self.entries.len())
            .filter(|&i| self.entries[i].is_live())
            .collect();
        let mut rec = Rec::new(self.mark(), std::mem::take(&mut self.log.snapshots));
        let mut out = Outcome::default();
        let engine = Engine {
            entries: &self.entries,
            index: &self.index,
            ctx: Ctx {
                pin: self.pin,
                limits: self.limits,
            },
        };
        // 체크포인트가 든 묶음은 증분으로 오지 않으므로 여기서는 후보가 없습니다.
        engine.segment(idx, &mut self.machine, &mut out, &mut rec);
        self.record(rec, out.stuck);
        self.pin_genesis();
        self.applied.extend_from_slice(&out.applied);
        self.rejected.extend(out.rejected.iter().cloned());
        self.key_events.extend_from_slice(&out.key_events);
        ResolveUpdate {
            full: false,
            applied: out.applied,
            rejected: out.rejected,
            unapplied: Vec::new(),
            key_events: out.key_events,
            state_changed: out.changed,
            checkpoints: Vec::new(),
            conflicts: Vec::new(),
            needs_update: out.needs_update,
        }
    }

    /// 새 항목이 들어와도 처리 기록이 그대로 맞는 앞부분의 길이 p를 셉니다(새 항목을 등록하기 전에 부름).
    ///
    /// 왜 p 앞은 그대로인가: 해소는 순서 연결을 준비되는 대로 먼저 처리하고(상태를 바꾸지 않음), 그다음 준비된
    /// 이벤트 가운데 정렬 키가 가장 작은 것을 고릅니다. 정렬 키는 지금 Machine만의 함수입니다(순위가 바뀌는 범위를
    /// 빠짐없이 남겨 준비 집합의 키가 늘 최신, `RankChange`). 그래서 같은 항목 집합이면 이벤트를 고르는 순서가 하나로
    /// 정해지고, 기록의 앞부분 `order[..p]`는 그 동안의 준비 집합이 새 항목 때문에 달라지지 않으면 그대로 맞습니다.
    /// p 앞에서는 (가) 새 항목은 이미 처리된 부모가 다 끝나기 전이라 준비될 수 없고, (나) 새 ID를 빠진 부모(충족된
    /// 것으로 봄)로 두었던 항목은 이제 새 항목을 기다려야 하는데 그 항목과 자손은 p 뒤에 있으며, (다) 본문을 받은
    /// 순서 연결은 준비되자마자 이벤트를 고르기 전에 처리됐으므로 p 앞의 어느 선택 때도 준비 집합에 없었습니다.
    /// 논리 시계 검사도 p 앞 항목은 부모가 바뀌지 않아 그대로입니다. 따라서 p 이하의 스냅숏에서 나머지를 다시 처리하면
    /// 처음부터 해소한 것과 적용·거부·순서·상태가 같습니다. 경계 아래 기록(`Floor`)은 처리된 것으로 봅니다.
    fn replay_start(
        &self,
        events: &[DecodedEvent],
        links: &[LinkEvent],
        floors: &[(EventId, u64, bool)],
        upgraded: &[(usize, DecodedEvent)],
    ) -> usize {
        let new: HashSet<EventId> = events
            .iter()
            .map(|e| e.id)
            .chain(links.iter().map(|l| l.id))
            .chain(floors.iter().map(|f| f.0))
            .collect();
        let done = |i: usize| (self.log.pos[i] != NOT_DONE).then_some(self.log.pos[i]);
        let mut p = self.log.order.len();
        let parents = events
            .iter()
            .map(|e| e.header.parents.as_slice())
            .chain(links.iter().map(|l| l.parents.as_slice()));
        for ps in parents {
            // (가) 묶음 안에 부모가 있으면 그 부모보다 먼저 준비될 수 없으므로 묶음의 뿌리만 보면 됩니다.
            if ps.iter().any(|q| new.contains(q)) {
                continue;
            }
            let mut after = 0;
            for q in ps {
                if let Some(&i) = self.index.get(q) {
                    if !self.entries[i].is_live() {
                        continue;
                    }
                    let Some(pos) = done(i) else { return 0 };
                    after = after.max(pos + 1);
                }
            }
            p = p.min(after);
        }
        // (나)
        for id in &new {
            for &c in self.waiting.get(id).into_iter().flatten() {
                let Some(pos) = done(c) else { return 0 };
                p = p.min(pos);
            }
        }
        // (다)
        for (i, _) in upgraded {
            let Some(pos) = done(*i) else { return 0 };
            p = p.min(pos);
        }
        p
    }

    /// 처리 기록에서 `p` 이하의 마지막 스냅숏으로 돌아가 나머지와 새 항목(`entries[start..]`)을 다시 처리합니다.
    /// 다시 처리할 범위에 체크포인트가 있으면 그 판정은 바탕부터의 조상이 필요하므로 마지막 바탕부터, `p`가 마지막
    /// 바탕 앞이면 창의 바탕부터 다시 해소합니다.
    fn replay(&mut self, p: usize, start: usize) -> ResolveUpdate {
        let new_live: Vec<usize> = (start..self.entries.len())
            .filter(|&i| self.entries[i].is_live())
            .collect();
        let seg_at = self.segment_at();
        let again = &self.log.order[p.min(self.log.order.len())..];
        let has_checkpoint = again
            .iter()
            .chain(&new_live)
            .any(|&i| self.entries[i].is_checkpoint());
        // 다시 처리할 범위에 지금 바탕을 바로 잇지 않는 체크포인트(겨뤄 진 것, 진 쪽을 잇는 것)가 있으면 빠졌던 조상이
        // 와서 판정이 바뀔 수 있습니다. 새로 온 체크포인트와 같이 창의 바탕부터 다시 판정합니다(바탕 뒤 스냅숏에서 다시
        // 처리하면 바탕이 이미 정해져 있어, 처음부터 해소하는 노드와 다른 바탕을 고를 수 있었음).
        if has_checkpoint && again.iter().any(|&i| self.off_chain_entry(i)) {
            return self.rebuild();
        }
        let p = if has_checkpoint { p.min(seg_at) } else { p };
        if p < seg_at {
            return self.rebuild();
        }
        let (mark, mut machine) = if p == self.log.order.len() && !has_checkpoint {
            // 기록 전체가 그대로면(예: 동시 갈래를 그 갈래의 마지막 이벤트 뒤로 잇기) 지금 Machine이 곧 p의 상태라
            // 스냅숏으로 돌아가지 않고 새 항목만 처리합니다.
            (self.mark(), self.machine.clone())
        } else {
            match self.log.snapshots.iter().rposition(|s| s.mark.at <= p) {
                Some(k) => {
                    self.log.snapshots.truncate(k + 1);
                    let snap = &self.log.snapshots[k];
                    (snap.mark, snap.machine.clone())
                }
                None => match self.segment_start() {
                    Some((mark, m)) => {
                        self.log.snapshots.clear();
                        (mark, Some(m))
                    }
                    None => return self.rebuild(),
                },
            }
        };
        self.stats.replay += 1;
        // 다시 처리할 항목: 스냅숏 뒤에 처리했던 것과 새 항목. 기록이 멀쩡하면 그 밖에 처리하지 않은 항목은 없습니다.
        let mut idx = self.log.order.split_off(mark.at);
        for &i in &idx {
            self.log.pos[i] = NOT_DONE;
        }
        idx.extend(new_live);
        self.log.bases.retain(|b| b.mark.at <= mark.at);
        self.stats.replayed += idx.len() as u64;
        let mut rec = Rec::new(mark, std::mem::take(&mut self.log.snapshots));
        let mut out = Outcome::default();
        let engine = Engine {
            entries: &self.entries,
            index: &self.index,
            ctx: self.ctx(),
        };
        engine.segment(idx, &mut machine, &mut out, &mut rec);
        self.record(rec, out.stuck);
        let update = diff(
            &self.applied[mark.applied..],
            &self.rejected[mark.rejected..],
            &self.key_events[mark.key_events..],
            &out,
            self.machine.as_ref().map(|m| &m.st) != machine.as_ref().map(|m| &m.st),
        );
        self.machine = machine;
        self.applied.truncate(mark.applied);
        self.applied.extend(out.applied);
        self.rejected.truncate(mark.rejected);
        self.rejected.extend(out.rejected);
        self.key_events.truncate(mark.key_events);
        self.key_events.extend(out.key_events);
        self.pin_genesis();
        update
    }

    /// 새 항목 각각이 기존 머리 전부를 조상으로 두는지(묶음 안 경로 포함) 확인합니다.
    fn covers_heads(&self, events: &[DecodedEvent], links: &[LinkEvent]) -> bool {
        if self.heads.is_empty() {
            return true;
        }
        if self.heads.len() > MAX_COVER_HEADS {
            return false;
        }
        let head_idx: HashMap<EventId, usize> = self
            .heads
            .iter()
            .enumerate()
            .map(|(i, h)| (*h, i))
            .collect();
        let words = self.heads.len().div_ceil(64);
        let items: Vec<(EventId, &[EventId])> = events
            .iter()
            .map(|e| (e.id, e.header.parents.as_slice()))
            .chain(links.iter().map(|l| (l.id, l.parents.as_slice())))
            .collect();
        let n = items.len();
        let pos: HashMap<EventId, usize> = items
            .iter()
            .enumerate()
            .map(|(i, (id, _))| (*id, i))
            .collect();
        let mut indeg = vec![0u32; n];
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (i, (id, parents)) in items.iter().enumerate() {
            let mut ps = parents.to_vec();
            ps.sort_unstable();
            ps.dedup();
            for p in ps {
                if let Some(&j) = pos.get(&p) {
                    if p == *id {
                        return false;
                    }
                    indeg[i] += 1;
                    children[j].push(i);
                }
            }
        }
        let mut cover = vec![Vec::new(); n];
        let mut stack: Vec<usize> = (0..n).filter(|&i| indeg[i] == 0).collect();
        let mut seen = 0usize;
        while let Some(i) = stack.pop() {
            seen += 1;
            let mut c = vec![0u64; words];
            for p in items[i].1 {
                if let Some(&h) = head_idx.get(p) {
                    c[h / 64] |= 1u64 << (h % 64);
                } else if let Some(&j) = pos.get(p) {
                    for (w, v) in c.iter_mut().zip(&cover[j]) {
                        *w |= *v;
                    }
                }
            }
            cover[i] = c;
            for &ch in &children[i] {
                indeg[ch] -= 1;
                if indeg[ch] == 0 {
                    stack.push(ch);
                }
            }
        }
        if seen != n {
            return false;
        }
        let total = self.heads.len();
        let full: Vec<u64> = (0..words)
            .map(|w| {
                let bits = (total - w * 64).min(64);
                if bits == 64 {
                    u64::MAX
                } else {
                    (1u64 << bits) - 1
                }
            })
            .collect();
        cover.iter().all(|c| *c == full)
    }

    /// 창의 바탕부터 다시 해소하고 이전 결과와의 차이를 돌려줍니다(체크포인트가 없으면 처음부터).
    pub fn rebuild(&mut self) -> ResolveUpdate {
        self.stats.full += 1;
        let idx: Vec<usize> = (0..self.entries.len())
            .filter(|&i| self.entries[i].is_live())
            .collect();
        self.log = Log {
            pos: vec![NOT_DONE; self.entries.len()],
            ..Log::default()
        };
        let mut rec = Rec::new(Mark::default(), Vec::new());
        let mut machine = self.window.as_ref().map(|w| w.machine.clone());
        let mut out = Outcome::default();
        let engine = Engine {
            entries: &self.entries,
            index: &self.index,
            ctx: self.ctx(),
        };
        engine.segment(idx, &mut machine, &mut out, &mut rec);
        self.record(rec, out.stuck);
        let update = diff(
            &self.applied,
            &self.rejected,
            &self.key_events,
            &out,
            self.machine.as_ref().map(|m| &m.st) != machine.as_ref().map(|m| &m.st),
        );
        self.machine = machine;
        self.applied = out.applied;
        self.rejected = out.rejected;
        self.key_events = out.key_events;
        self.pin_genesis();
        update
    }

    /// 바탕이 둘 이상이면 앞 바탕을 창의 바탕으로 삼고 그 앞 항목을 RAM에서 버립니다. 남은 항목의 부모·마지막 바탕의
    /// 경계 위 조상은 논리 시계만 남깁니다(논리 시계 판정용). 겨루는 체크포인트를 판정할 수 있도록 마지막 바탕의 앞
    /// 바탕까지는 남깁니다. 버렸으면 참이고, 부르는 쪽이 머리·빠진 부모를 다시 셉니다([`Resolver::rebuild_dag`]).
    fn compact(&mut self) -> bool {
        let mut done = false;
        while self.log.bases.len() >= 2 && !self.log.broken {
            let slot = self.log.bases.remove(0);
            let cut = slot.mark;
            let Some(base) = slot.machine.base.clone() else {
                return done;
            };
            done = true;
            let n = self.entries.len();
            let mut keep = vec![false; n];
            for &i in &self.log.order[cut.at..] {
                keep[i] = true;
            }
            let mut floor_ids: HashSet<EventId> = base.frontier.iter().map(|f| f.id).collect();
            floor_ids.insert(base.event);
            for &i in &self.log.order[cut.at..] {
                floor_ids.extend(self.entries[i].parents().iter().copied());
            }
            let mut as_floor = vec![false; n];
            for id in &floor_ids {
                if let Some(&j) = self.index.get(id)
                    && !keep[j]
                {
                    as_floor[j] = true;
                }
            }
            let old = std::mem::take(&mut self.entries);
            let mut remap = vec![NOT_DONE; n];
            for (j, e) in old.into_iter().enumerate() {
                if keep[j] {
                    remap[j] = self.entries.len();
                    self.entries.push(e);
                } else if as_floor[j] {
                    remap[j] = self.entries.len();
                    self.entries.push(Entry::Floor {
                        id: e.id(),
                        lamport: e.lamport(),
                    });
                }
            }
            self.stats.compacted += (n - self.entries.len()) as u64;
            self.index = self
                .entries
                .iter()
                .enumerate()
                .map(|(i, e)| (e.id(), i))
                .collect();
            let order: Vec<usize> = self.log.order[cut.at..].iter().map(|&i| remap[i]).collect();
            self.log.pos = vec![NOT_DONE; self.entries.len()];
            for (k, &e) in order.iter().enumerate() {
                self.log.pos[e] = k;
            }
            self.log.order = order;
            for s in &mut self.log.snapshots {
                s.mark = s.mark.minus(cut);
            }
            for b in &mut self.log.bases {
                b.mark = b.mark.minus(cut);
                b.entry = remap[b.entry];
            }
            self.window = Some(Box::new(BaseSlot {
                mark: Mark::default(),
                entry: remap[slot.entry],
                machine: slot.machine,
            }));
            self.applied.drain(..cut.applied);
            self.rejected.drain(..cut.rejected);
            self.key_events.drain(..cut.key_events);
        }
        done
    }

    pub fn state(&self) -> Option<&SpaceState> {
        self.machine.as_ref().map(|m| &m.st)
    }

    pub fn space(&self) -> Option<SpaceId> {
        self.machine.as_ref().map(|m| m.space)
    }

    /// 적용된 공간 생성 이벤트(고정했다면 그 값).
    pub fn genesis(&self) -> Option<EventId> {
        self.machine.as_ref().map(|m| m.genesis).or(self.pin)
    }

    /// 멤버마다 (마지막으로) 들어온 논리 시계. 지속 맵이라 복제가 O(1)입니다(상태가 바뀔 때마다 3만 명을 다시 모으지
    /// 않게).
    pub fn member_since_map(&self) -> yangpa_core::model::SharedMap<UserId, u64> {
        self.machine
            .as_ref()
            .map(|m| m.member_since.clone())
            .unwrap_or_default()
    }

    pub fn member_since(&self, user: &UserId) -> Option<u64> {
        self.machine
            .as_ref()
            .and_then(|m| m.member_since.get(user).copied())
    }

    /// 적용된 이벤트(창 안의 것: 창의 바탕 앞은 버림).
    pub fn applied(&self) -> &[EventId] {
        &self.applied
    }

    pub fn rejected(&self) -> &[(EventId, AuthError)] {
        &self.rejected
    }

    pub fn key_events(&self) -> &[EventId] {
        &self.key_events
    }

    /// 지금까지의 결과 전체(복사본, 창 안의 것).
    pub fn resolved(&self) -> Resolved {
        Resolved {
            state: self.state().cloned(),
            applied: self.applied.clone(),
            rejected: self.rejected.clone(),
            key_events: self.key_events.clone(),
            member_since: self
                .machine
                .as_ref()
                .map(|m| m.member_since.clone())
                .unwrap_or_default(),
            genesis: self.machine.as_ref().map(|m| m.genesis),
            keys: self.key_view(),
        }
    }

    /// 아는 ID인지(경계 아래로 논리 시계만 아는 것 포함).
    pub fn contains(&self, id: &EventId) -> bool {
        self.index.contains_key(id)
    }

    /// 알고 있는 항목(이벤트 + 순서 연결 + 경계 아래 기록) 수.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 아무도 부모로 가리키지 않은 항목(정렬됨).
    pub fn heads(&self) -> Vec<EventId> {
        self.heads.iter().copied().collect()
    }

    /// 부모로 가리켜졌지만 아직 받지 않은 ID(정렬됨).
    pub fn missing(&self) -> Vec<EventId> {
        let mut v: Vec<EventId> = self.waiting.keys().copied().collect();
        v.sort_unstable();
        v
    }

    /// 빠진 부모가 있는지([`Resolver::missing`]을 만들지 않고).
    pub fn has_missing(&self) -> bool {
        !self.waiting.is_empty()
    }

    /// 이 ID를 빠진 부모로 기다리는지.
    pub fn waits_for(&self, id: &EventId) -> bool {
        self.waiting.contains_key(id)
    }

    pub fn stats(&self) -> ResolverStats {
        self.stats
    }

    /// 지금 바탕 체크포인트(없으면 `None`).
    pub fn base(&self) -> Option<CheckpointBase> {
        self.machine
            .as_ref()
            .and_then(|m| m.base.as_deref())
            .cloned()
    }

    /// 지금 바탕 체크포인트 직후 상태의 정규 인코딩(스냅숏 평문). 데몬이 저장해 두고 재시작·시드에 씁니다.
    pub fn base_snapshot(&self) -> Option<Vec<u8>> {
        let m = self.base_machine()?;
        m.base.as_ref()?;
        Some(m.encode())
    }

    /// 지금 바탕 체크포인트 직후의 상태와 멤버 입장 시점. 같은 바탕을 받은 멤버는 모두 같은 값을 얻으므로, 보관 노드에
    /// 보낼 바탕 명단을 누가 만들어도 같습니다(그 위의 차이를 다른 멤버가 이어 보낼 수 있게). 바탕이 없으면 `None`.
    pub fn base_state(&self) -> Option<(SpaceState, SharedMap<UserId, u64>)> {
        let m = self.base_machine()?;
        m.base.as_ref()?;
        Some((m.st.clone(), m.member_since.clone()))
    }

    /// 바탕 체크포인트의 기기 트리 잎 증명표(`GetDeviceKeys`에 답할 때). 바탕이 없으면 `None`.
    pub fn device_proofs(&self) -> Option<crate::checkpoint::DeviceProofs> {
        let m = self.base_machine()?;
        let base = m.base.as_ref()?;
        Some(crate::checkpoint::DeviceProofs::of(base.id, m))
    }

    /// 지금 바탕 체크포인트 직후의 상태(바탕이 처리 기록에 있으면 그것, 아니면 창의 바탕).
    fn base_machine(&self) -> Option<&Machine> {
        match self.log.bases.last() {
            Some(b) => Some(&b.machine),
            None => self.window.as_ref().map(|w| &w.machine),
        }
    }

    /// 마지막 바탕 뒤에 처리한 항목 수(바탕이 없으면 전부). 데몬이 체크포인트를 낼 때인지 볼 때 씁니다.
    pub fn since_base(&self) -> usize {
        self.log.order.len() - self.segment_at()
    }

    /// 체크포인트가 은퇴시킨 상태 키인지. 데몬은 새 멤버에게 은퇴한 키를 봉인해 주지 않습니다(그 키로 쓴 이벤트는 모두
    /// 체크포인트 아래라 새 멤버가 열 일이 없음).
    pub fn is_retired_state_key(&self, k: &StateKeyId) -> bool {
        self.machine.as_ref().is_some_and(|m| m.is_retired(k))
    }

    /// 바탕 뒤 상태 이벤트가 쓴 상태 키 수(바탕의 유효 키 포함). [`MAX_VALID_STATE_KEYS`]에 가까우면 데몬이 체크포인트를
    /// 내 키 목록을 줄입니다(새 멤버 키 묶음이 크루 나이와 무관하게 작게).
    pub fn state_keys_in_use(&self) -> usize {
        self.machine.as_ref().map_or(0, |m| m.state_keys_in_use())
    }

    /// 상태에 있는 체크포인트 작성 위임.
    pub fn delegation(&self) -> Option<DelegationCert> {
        self.machine.as_ref().and_then(|m| m.delegation().cloned())
    }

    /// 크루 키 사슬의 머리(체크포인트 본문에 싣는 크루 [`KeyHead`]). 갱신이 아직 없으면 `None`.
    pub fn crew_key_head(&self) -> Option<KeyHead> {
        self.machine.as_ref().and_then(|m| m.crew_head())
    }

    /// 체크포인트를 만들 값(작성 기기용). 지금 머리를 부모로, 머리가 이번 구간의 모든 이벤트를 덮고 빠진 부모·풀지 못한
    /// 상태 이벤트가 없을 때만 만듭니다. `current_key`는 체크포인트 이벤트를 암호화할 지금 상태 키입니다(유효한 키에
    /// 경계 위 이벤트가 쓴 키와 함께 넣음).
    pub fn checkpoint_draft(
        &self,
        current_key: StateKeyId,
    ) -> Result<CheckpointDraft, &'static str> {
        let machine = self.machine.as_ref().ok_or("공간이 없음")?;
        if self.log.broken {
            return Err("처리 기록을 다시 세우는 중");
        }
        if !self.waiting.is_empty() {
            return Err("빠진 부모가 있음");
        }
        let heads: Vec<EventId> = self.heads.iter().copied().collect();
        if heads.is_empty() || heads.len() > yangpa_core::event::MAX_PARENTS {
            return Err("머리 수");
        }
        let base = machine.base.as_ref();
        let seg = &self.log.order[self.segment_at()..];
        let floor = base.map_or(0, |b| b.floor);
        let live: Vec<usize> = seg
            .iter()
            .copied()
            .filter(|&i| self.entries[i].lamport() >= floor)
            .collect();
        if live
            .iter()
            .any(|&i| matches!(&self.entries[i], Entry::Link(l) if l.undecoded))
        {
            return Err("풀지 못한 상태 이벤트가 있음");
        }
        // 머리의 조상이 이번 구간 전부여야 지금 상태가 곧 체크포인트 조상만의 상태입니다.
        let set: HashSet<usize> = live.iter().copied().collect();
        let mut seen = HashSet::new();
        let mut stack = heads.clone();
        while let Some(id) = stack.pop() {
            if let Some(&j) = self.index.get(&id)
                && set.contains(&j)
                && seen.insert(j)
            {
                stack.extend_from_slice(self.entries[j].parents());
            }
        }
        if seen.len() != set.len() {
            return Err("머리가 이번 구간을 덮지 않음");
        }
        let mut maxlam = 0;
        for h in &heads {
            maxlam = maxlam.max(
                self.index
                    .get(h)
                    .map(|&i| self.entries[i].lamport())
                    .ok_or("모르는 머리")?,
            );
        }
        let mut f = floor.max(maxlam.saturating_sub(FRONTIER_DEPTH));
        let frontier = loop {
            let fr = frontier_of(&self.entries, live.iter().copied(), base, f);
            if fr.len() <= MAX_FRONTIER {
                break fr;
            }
            if f >= maxlam {
                return Err("경계 위 조상이 너무 많음");
            }
            f += 1;
        };
        let mut valid: BTreeSet<StateKeyId> = BTreeSet::new();
        valid.insert(current_key);
        for e in &frontier {
            if let Some(&i) = self.index.get(&e.id)
                && let Entry::Event(b) = &self.entries[i]
                && let KeyRef::State(k) = b.0.header.key
            {
                valid.insert(k);
            }
        }
        let valid: Vec<StateKeyId> = valid.into_iter().take(MAX_VALID_STATE_KEYS).collect();
        if !valid.contains(&current_key) {
            return Err("지금 상태 키가 유효한 키에 들지 않음");
        }
        let seq = base.map_or(1, |b| b.seq + 1);
        let mut m = machine.clone();
        let sealed = m.seal(seq, &valid);
        let signer_proofs = self.signer_proof_candidates(&sealed.owner_chain);
        let d = m.digest();
        let owner = m.st.owner;
        let owner_record =
            m.st.devices
                .get(&owner)
                .cloned()
                .ok_or("소유자 기기 목록 없음")?;
        Ok(CheckpointDraft {
            seq,
            prev: base.map(|b| b.id),
            heads,
            lamport: maxlam + 1,
            frontier_lamport: f,
            frontier,
            state_hash: d.state_hash,
            user_root: d.user_root,
            users: d.users,
            device_root: d.device_root,
            devices: d.devices,
            key_heads: sealed.crew_head.into_iter().collect(),
            valid_state_keys: valid,
            owner,
            owner_record,
            owner_proof: d.owner_proof.ok_or("소유자가 멤버가 아님")?,
            revoked: sealed.revoked,
            revocation_root: sealed.revocation_root,
            owner_chain: sealed.owner_chain,
            delegation_hash: sealed.delegation_hash,
            signer_proofs,
            snapshot: d.snapshot,
        })
    }

    /// 소유자가 바뀐 구간의 서명 기기 증명 후보(바탕 기기 트리에서): 두 번째 이후 인증서의 기기, 새 소유자의 활성 기기.
    /// 바탕에 없던 기기는 증명이 없어, 그 체크포인트로는 새 멤버가 처음부터 따라잡습니다.
    fn signer_proof_candidates(&self, certs: &[OwnershipCert]) -> Vec<DeviceLeafProof> {
        let Some(last) = certs.last() else {
            return Vec::new();
        };
        let Some(m) = self.base_machine() else {
            return Vec::new();
        };
        let Some(base) = m.base.as_ref() else {
            return Vec::new();
        };
        let proofs = crate::checkpoint::DeviceProofs::of(base.id, m);
        let mut want: Vec<(DeviceId, UserId)> =
            certs[1..].iter().map(|c| (c.device, c.from)).collect();
        if let Some(rec) = m.st.devices.get(&last.to) {
            want.extend(rec.active().map(|d| (d.device, last.to)));
        }
        want.sort();
        want.dedup();
        want.iter()
            .filter_map(|(d, u)| {
                proofs
                    .proof(d)
                    .filter(|p| p.user == *u && p.record.revoked_at.is_none())
            })
            .collect()
    }

    /// 머리와 그 논리 시계(경계 위). 데몬은 공간 스트림 이벤트의 부모를 이것으로 고릅니다: 저장소 머리는 경계 아래
    /// 이벤트가 진짜 머리를 가릴 수 있음.
    pub fn head_lamports(&self) -> Vec<(EventId, u64)> {
        self.heads
            .iter()
            .filter_map(|h| self.index.get(h).map(|&i| (*h, self.entries[i].lamport())))
            .collect()
    }

    /// 기준점(`anchor`)과 그 조상만으로 해소한 상태. 부분 전달 이벤트(받은편지함·키 그룹)의 권한을 이것으로
    /// 검사합니다: 해소는 항목 집합만의 함수이고 조상 집합은 해시로 고정되므로, 그 뒤에 무엇을 더 받았든 어느 노드에서나
    /// 같은 답이 나옵니다.
    ///
    /// 체크포인트가 있으면 경계 규칙을 따릅니다: 바탕의 조상이 아닌 것은 모두 바탕 뒤에 놓이므로, 기준점마다 그것을
    /// 처리한 구간의 바탕(그 앞에 놓인 가장 늦은 바탕)을 찾아 그 가운데 가장 늦은 바탕 뒤 상태에서 시작하고, 그 뒤에
    /// 처리한 조상만 이어 처리합니다. 바탕과 동시인 기준점도 그 바탕 뒤 상태에서 세므로, 처음부터 아는 노드와 그 바탕으로
    /// 들어온 멤버가 같은 답을 얻습니다. 창의 바탕 앞에서 처리한 기준점(경계 아래 기록)은 이 노드가 셀 수 없어 모르는
    /// 기준점으로 봅니다.
    ///
    /// `allow_gaps`가 거짓이면 기준점이나 그 조상에 모르는 이벤트, 본문을 기다리는 상태 이벤트([`LinkEvent::undecoded`])가
    /// 있을 때 [`AnchorState::Incomplete`]입니다. 참이면 해소기처럼 그것들을 빈 구간으로 보고 해소합니다(모르는 기준점은
    /// 빼고, 하나도 모르면 여전히 `Incomplete`). 영영 오지 않는 부모 때문에 기다리기만 하지 않게, 데몬이 따라잡기를 한
    /// 바퀴 돈 뒤에만 씁니다.
    pub fn state_at(&self, anchor: &[EventId], allow_gaps: bool) -> AnchorState {
        match self.machine_at(anchor, allow_gaps) {
            Err(incomplete) => incomplete,
            Ok((machine, gaps)) => AnchorState::Ready {
                state: machine.map(|m| Box::new(m.st)),
                gaps,
            },
        }
    }

    /// [`Resolver::state_at`]과 같되 키 사슬까지 셉니다(키 그룹 사슬 판정용). 기준점 ID를 하나라도 모르거나 공간이
    /// 아직 없으면 `None`. 조상에 빠진 부모나 본문을 풀지 못한 상태 이벤트가 있으면 그것을 빈 구간으로 보고 세운 뒤
    /// `true`(빈 구간)를 함께 돌려줍니다. 빈 구간 없이만 세우면 아무도 열 수 없는 상태 이벤트 하나가 그 뒤 모든 기준점을
    /// 영영 막습니다. 빈 구간을 낀 판정은 [`Resolver::gap_epoch`]가 바뀌면 다시 합니다.
    pub fn point_at(&self, anchor: &[EventId]) -> Option<(KeyPoint, bool)> {
        if anchor.iter().any(|a| !self.index.contains_key(a)) {
            return None;
        }
        let (machine, gaps) = self.machine_at(anchor, true).ok()?;
        machine.map(|m| (m.point(), gaps))
    }

    /// 이미 아는 이벤트의 조상 집합이 바뀐 횟수([`Resolver::point_at`]의 빈 구간 판정을 다시 할 때).
    pub fn gap_epoch(&self) -> u64 {
        self.gap_epoch
    }

    /// 기준점 가운데 바탕 체크포인트 경계 F 아래라고 아는 것이 있는지(압축하고 남은 경계 자리 포함). 키 그룹 사슬이
    /// 늦은 갱신을 거를 때 씁니다.
    pub fn below_floor(&self, anchor: &[EventId]) -> bool {
        let floor = self.floor();
        floor > 0
            && anchor.iter().any(|a| {
                self.index
                    .get(a)
                    .is_some_and(|&i| self.entries[i].lamport() < floor)
            })
    }

    /// 기준점 이벤트 하나가 지금 바탕 체크포인트 C에 비춰 어디인지([`AnchorPlace`]). C 자신과 C 뒤에 놓인
    /// 이벤트는 `After`, C의 경계 위 조상(본문의 경계 목록)은 `Covered`, 경계 F 아래는 `Below`입니다. 경계 위 조상은 본문이
    /// 모두 싣고 C 뒤 이벤트는 모든 노드가 받으므로, 처음부터 아는 노드와 체크포인트로 들어온 노드가 같이 셉니다(F 아래
    /// 이벤트는 새 멤버가 모를 수 있어 `Unknown`일 수 있음). 바탕이 없으면 아는 이벤트는 모두 `After`.
    pub fn anchor_place(&self, id: &EventId) -> AnchorPlace {
        let Some(&i) = self.index.get(id) else {
            return AnchorPlace::Unknown;
        };
        let Some(b) = self.machine.as_ref().and_then(|m| m.base.as_deref()) else {
            return AnchorPlace::After;
        };
        if *id == b.event {
            return AnchorPlace::After;
        }
        if self.entries[i].lamport() < b.floor {
            return AnchorPlace::Below;
        }
        if b.frontier.binary_search_by(|f| f.id.cmp(id)).is_ok() {
            return AnchorPlace::Covered;
        }
        AnchorPlace::After
    }

    /// 지금 사슬에서 받아들인 크루 갱신(순번)의 조각 ID(조각 번호 순, 최근 64개까지).
    pub fn crew_parts(&self, seq: u64) -> Option<std::sync::Arc<[EventId]>> {
        self.machine.as_ref()?.keys.parts_of(seq)
    }

    /// 지금 상태와 키 사슬.
    pub fn point(&self) -> Option<KeyPoint> {
        self.machine.clone().map(Machine::point)
    }

    /// 지금 크루 키 사슬과 기기 주인.
    pub fn key_view(&self) -> Option<KeyView> {
        self.machine.as_ref().map(Machine::key_view)
    }

    /// `earlier`가 모두 `later` 가운데 하나이거나 그 조상인지(해소기가 아는 DAG로).
    pub fn includes(&self, later: &[EventId], earlier: &[EventId]) -> bool {
        let dag = EntriesDag {
            entries: &self.entries,
            index: &self.index,
        };
        ancestry::reaches(&dag, later, earlier)
    }

    /// 크루 키 갱신에 넣을 수 있는 가입 기기: 활성 기기가 된 이벤트가 `parents`(갱신 머리 조각이 둘 부모) 가운데
    /// 하나이거나 그 조상인 기기만 `devices` 순서대로 돌려줍니다. 멤버는 넣은 기기마다 이것을 요구하므로(조상이 아니면
    /// 갱신 전체를 거부), 동시 가입이 많아 머리가 부모 상한을 넘을 때 확정 기기가 닿는 것만 먼저 넣는 데 씁니다.
    pub fn joins_reachable(
        &self,
        parents: &[EventId],
        devices: &[yangpa_core::ids::DeviceId],
    ) -> Vec<yangpa_core::ids::DeviceId> {
        let Some(m) = self.machine.as_ref() else {
            return Vec::new();
        };
        let by: Vec<(yangpa_core::ids::DeviceId, EventId)> = devices
            .iter()
            .filter_map(|d| m.keys.added_by.get(d).map(|e| (*d, *e)))
            .collect();
        let targets: Vec<EventId> = by
            .iter()
            .map(|(_, e)| *e)
            .filter(|e| *e != m.genesis)
            .collect();
        let dag = EntriesDag {
            entries: &self.entries,
            index: &self.index,
        };
        let ok = ancestry::reached(&dag, parents, &targets);
        by.into_iter()
            .filter(|(_, e)| *e == m.genesis || ok.contains(e))
            .map(|(d, _)| d)
            .collect()
    }

    /// 기준점과 그 조상만으로 해소한 Machine과 빈 구간 여부. `Err`는 [`AnchorState::Incomplete`].
    fn machine_at(
        &self,
        anchor: &[EventId],
        allow_gaps: bool,
    ) -> Result<(Option<Machine>, bool), AnchorState> {
        // 기준점 자신을 모르면(빈 구간을 허용하지 않을 때) 조상을 따라가 볼 것도 없습니다(데몬이 자주 다시 묻는 경우라
        // 싸게 끝냄). 창의 바탕 앞 기록(바탕 이벤트 말고)도 셀 수 없으므로 모르는 것으로 봅니다.
        let known = |a: &EventId| {
            self.index
                .get(a)
                .is_some_and(|&i| self.anchor_segment(i, self.log.broken).is_some())
        };
        let mut unknown: Vec<EventId> = anchor.iter().filter(|a| !known(a)).copied().collect();
        if !unknown.is_empty() && (!allow_gaps || unknown.len() == anchor.len()) {
            unknown.sort_unstable();
            unknown.dedup();
            return Err(AnchorState::Incomplete {
                unknown,
                undecoded: Vec::new(),
            });
        }
        let n = self.entries.len();
        let mut inside = vec![false; n];
        let roots: Vec<usize> = anchor
            .iter()
            .filter(|a| known(a))
            .filter_map(|a| self.index.get(a))
            .copied()
            .collect();
        let mut stack = roots.clone();
        let mut undecoded = Vec::new();
        let mut count = 0usize;
        while let Some(i) = stack.pop() {
            if inside[i] {
                continue;
            }
            inside[i] = true;
            count += 1;
            if let Entry::Link(l) = &self.entries[i]
                && l.undecoded
            {
                undecoded.push(l.id);
            }
            for p in self.entries[i].parents() {
                match self.index.get(p) {
                    Some(&j) if !inside[j] => stack.push(j),
                    Some(_) => {}
                    None => unknown.push(*p),
                }
            }
        }
        let gaps = !unknown.is_empty() || !undecoded.is_empty();
        if !allow_gaps && gaps {
            unknown.sort_unstable();
            unknown.dedup();
            undecoded.sort_unstable();
            return Err(AnchorState::Incomplete { unknown, undecoded });
        }
        if count == n {
            return Ok((self.machine.clone(), gaps));
        }
        let last = self.log.bases.len();
        let (start_at, start_machine, last_segment) = if self.log.broken {
            // 위치를 셀 수 없음: 조상 안의 가장 늦은 바탕, 없으면 창의 바탕(또는 처음).
            match self
                .log
                .bases
                .iter()
                .enumerate()
                .rev()
                .find(|(_, b)| inside[b.entry])
            {
                Some((k, b)) => (b.mark.at, Some(b.machine.clone()), k + 1 == last),
                None => (
                    0,
                    self.window.as_ref().map(|w| w.machine.clone()),
                    last == 0,
                ),
            }
        } else {
            // 기준점마다 처리한 구간 가운데 가장 늦은 것(`None`은 창의 바탕 또는 처음).
            let seg = roots
                .iter()
                .filter_map(|&i| self.anchor_segment(i, false))
                .max()
                .flatten();
            match seg {
                Some(k) => {
                    let b = &self.log.bases[k];
                    (b.mark.at, Some(b.machine.clone()), k + 1 == last)
                }
                None => (
                    0,
                    self.window.as_ref().map(|w| w.machine.clone()),
                    last == 0,
                ),
            }
        };
        let (mut machine, rest): (Option<Machine>, Vec<usize>) = if self.log.broken {
            let rest = (0..n)
                .filter(|&i| inside[i] && self.entries[i].is_live())
                .collect();
            (start_machine, rest)
        } else {
            // 처리 기록에서 조상 밖의 첫 항목 위치 q 앞까지는 조상만 해소할 때도 같은 순서로 처리됩니다(조상 밖 항목은 q
            // 앞의 어느 선택에서도 가장 작은 키가 아니었고, 조상 집합은 부모 쪽으로 닫혀 있어 준비 조건도 같음). 그래서 q
            // 이하의 스냅숏에서 나머지 조상만 이어 처리합니다([`Resolver::replay_start`]와 같은 논리).
            let q = self.log.order[start_at..]
                .iter()
                .position(|&i| !inside[i])
                .map_or(self.log.order.len(), |k| start_at + k);
            let snap = if last_segment {
                self.log
                    .snapshots
                    .iter()
                    .rposition(|s| s.mark.at <= q && s.mark.at >= start_at)
            } else {
                None
            };
            let (from, machine) = match snap {
                Some(k) => {
                    let s = &self.log.snapshots[k];
                    (s.mark.at, s.machine.clone())
                }
                None => (start_at, start_machine),
            };
            let rest = self.log.order[from..]
                .iter()
                .copied()
                .filter(|&i| inside[i])
                .collect();
            (machine, rest)
        };
        let engine = Engine {
            entries: &self.entries,
            index: &self.index,
            ctx: self.ctx(),
        };
        let mut out = Outcome::default();
        engine.run_plain(&rest, &mut machine, &HashMap::new(), &mut out, None);
        Ok((machine, gaps))
    }

    /// 항목 `i`를 처리한 구간: `Some(Some(k))`는 처리 기록의 바탕 `k` 뒤, `Some(None)`은 창의 바탕(또는 처음) 뒤, `None`은
    /// 창의 바탕 앞이라 셀 수 없음(경계 아래 기록, 처리하지 않은 항목). 바탕 이벤트는 그 바탕의 구간입니다(바탕 뒤
    /// 상태가 곧 그 상태). `broken`이면 위치를 볼 수 없어 창 안의 아는 항목을 모두 셀 수 있는 것으로 봅니다.
    fn anchor_segment(&self, i: usize, broken: bool) -> Option<Option<usize>> {
        if self.window.as_ref().is_some_and(|w| w.entry == i) {
            return Some(None);
        }
        if let Some(k) = self.log.bases.iter().position(|b| b.entry == i) {
            return Some(Some(k));
        }
        if !self.entries[i].is_live() {
            return None;
        }
        if broken {
            return Some(None);
        }
        let p = *self.log.pos.get(i)?;
        if p == NOT_DONE {
            return None;
        }
        Some(self.log.bases.iter().rposition(|b| b.mark.at <= p))
    }

    /// 이 ID를 본문을 기다리는 순서 연결로만 아는지([`LinkEvent::undecoded`]). 데몬이 기준점 상태를 다시 셀 때가 됐는지
    /// 볼 때 씁니다.
    pub fn awaiting_body(&self, id: &EventId) -> bool {
        self.index
            .get(id)
            .is_some_and(|&i| matches!(&self.entries[i], Entry::Link(l) if l.undecoded))
    }

    /// 마지막으로 적용된 상태 이벤트부터 거꾸로 최대 `n`개(부분 전달 이벤트의 기준점 후보). 저장소의 머리와 달리 다른
    /// 노드가 RAM에만 두거나 거부할 수 있는 키 레인 이벤트가 끼지 않습니다.
    pub fn recent_applied(&self, n: usize) -> Vec<EventId> {
        self.applied.iter().rev().take(n).copied().collect()
    }

    /// 시험 전용: 처리 기록이 지금 결과와 맞는지(위치표·스냅숏 위치와 결과 길이).
    #[cfg(test)]
    fn check_log(&self) {
        if self.log.broken {
            return;
        }
        let live = self.entries.iter().filter(|e| e.is_live()).count();
        assert_eq!(self.log.order.len(), live, "처리 기록 길이");
        for (k, &e) in self.log.order.iter().enumerate() {
            assert_eq!(self.log.pos[e], k, "위치표");
        }
        let mut last = 0;
        for s in &self.log.snapshots {
            assert!(s.mark.at > last && s.mark.at <= self.log.order.len());
            assert!(s.mark.applied <= self.applied.len());
            assert!(s.mark.rejected <= self.rejected.len());
            assert!(s.mark.key_events <= self.key_events.len());
            last = s.mark.at;
        }
        assert!(self.log.snapshots.len() <= MAX_SNAPSHOTS);
        assert!(self.log.bases.len() <= 1, "바탕은 압축 뒤 하나까지");
    }
}

/// 이전 결과(`old_*`)와 새 결과(`out`)의 차이. 스냅숏 재생이면 둘 다 같은 스냅숏 뒤의 부분만 넘깁니다: 앞부분은 같은
/// 항목이 같은 결과로 처리돼 차이가 없고, 뒷부분 항목은 앞부분에 없으므로 뒷부분끼리 견주면 전체를 견준 것과 같습니다.
fn diff(
    old_applied: &[EventId],
    old_rejected: &[(EventId, AuthError)],
    old_key_events: &[EventId],
    out: &Outcome,
    state_changed: bool,
) -> ResolveUpdate {
    let was_applied: HashSet<EventId> = old_applied.iter().copied().collect();
    let now_applied: HashSet<EventId> = out.applied.iter().copied().collect();
    let was_rejected: HashSet<EventId> = old_rejected.iter().map(|(id, _)| *id).collect();
    let was_key_event: HashSet<EventId> = old_key_events.iter().copied().collect();
    ResolveUpdate {
        full: true,
        applied: out
            .applied
            .iter()
            .filter(|id| !was_applied.contains(id))
            .copied()
            .collect(),
        rejected: out
            .rejected
            .iter()
            .filter(|(id, _)| !was_rejected.contains(id))
            .cloned()
            .collect(),
        unapplied: old_applied
            .iter()
            .filter(|id| !now_applied.contains(id))
            .copied()
            .collect(),
        key_events: out
            .key_events
            .iter()
            .filter(|id| !was_key_event.contains(id))
            .copied()
            .collect(),
        state_changed,
        checkpoints: Vec::new(),
        conflicts: Vec::new(),
        needs_update: out.needs_update,
    }
}
