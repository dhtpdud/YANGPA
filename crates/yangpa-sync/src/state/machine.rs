//! 이벤트 종류별 권한 검사와 상태 반영. 각 처리기는 모든 검사를 마친 뒤에만 상태를 바꿔서,
//! 거부된 이벤트가 상태를 절반만 바꾸는 일이 없습니다.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use yangpa_core::archive::{self, ArchiveEntry, ArchiveGrant};
use yangpa_core::body::{CustomEmoji, EventBody, InviteInfo};
use yangpa_core::checkpoint::{
    self as ckpt, DelegationCert, FrontierEntry, KeyHead, MAX_CHECKPOINT_REVOKED, MAX_OWNER_CHAIN,
    OwnershipCert, SignedCheckpoint,
};
use yangpa_core::device::{
    DeviceRecord, ListChange, MemberDevices, SignedDeviceList, SignedProfile,
};
use yangpa_core::emoji;
use yangpa_core::event::EventHeader;
use yangpa_core::ids::{
    BlobId, ChannelId, CheckpointId, DeviceId, EmojiId, EventId, KeyGroupId, RoleId, SpaceId,
    StateKeyId, UserId,
};
use yangpa_core::links::ContactInfo;
use yangpa_core::model::{
    BanInfo, ChannelKind, ChannelSettings, ChannelTransport, ChatTtl, CrewServers, EditRules,
    EmojiLimits, HistoryVisibility, InviteState, MAX_CREW_SERVER_LEN, MAX_CREW_SERVERS,
    MAX_VOLATILE_CAP, MIN_VOLATILE_CAP, Member, OverwriteTarget, PermissionOverwrite, Permissions,
    Role, RoleDisplay, SharedMap, SpaceKind, SpaceSettings, SpaceState, valid_attach_limit,
};

pub(super) mod keys;

use super::ancestry::Dag;
use super::authorize::{check_forum_update, check_free_text, check_name};
use super::facts::{DevicesFacts, JoinFacts, KeyFacts, ListFacts, Stored, StoredBody};
use super::limits::*;
use super::{
    ADMIN_ROLE_NAME, ADMIN_ROLE_RANK, AuthError, FRIEND_SPACE_EVERYONE, Limits, SigMemo,
    admin_role_id,
};
use keys::CrewKeys;

/// 이벤트 하나를 적용한 효과.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Effect {
    /// 공간 상태가 바뀜.
    pub changed: bool,
    /// 데몬이 처리할 키 이벤트(받아들인 크루 `KeyUpdate`의 마지막 조각, `KeyBackupSet`).
    pub key_event: bool,
}

impl Effect {
    pub fn changed() -> Self {
        Effect {
            changed: true,
            key_event: false,
        }
    }
    fn none() -> Self {
        Effect::default()
    }
    fn key_event() -> Self {
        Effect {
            changed: false,
            key_event: true,
        }
    }
}

mod checkpoint;

pub(crate) use checkpoint::check_owner_proof;

type R = Result<Effect, AuthError>;

/// 전환 전 채널의 과거 기록 공개 변경을 지금 반영할지.
enum Consent {
    Apply,
    /// 동의만 기록하고 기록 공개는 그대로 둠.
    Wait,
}

/// 적용 하나로 역할 순위가 바뀌었을 수 있는 범위. 해소기는 바뀐 사용자의 준비 항목만 정렬 키를 다시 계산해,
/// 동시 가입 K개가 매번 준비 집합 전체를 다시 정렬하는 O(K²)가 되지 않게 합니다.
///
/// 순위([`SpaceState::rank_of`])는 소유자, 멤버마다의 역할 목록, 역할의 등급(`Role::level`)으로만 정해집니다. 그래서
/// 이 셋을 바꾸는 곳은 모두 범위를 남겨야 합니다: 멤버 추가·제거·역할 목록 변경은 그 사용자, 소유자 변경은 옛·새
/// 소유자, 역할 등급 변경·역할 삭제·크루 전환은 전부(공간 생성도 해소기가 전부로 봄). 빠뜨리면 준비 집합의 정렬 키가
/// 옛 순위로 남아 처음부터 해소한 것과 순서가 달라지고, 스냅숏 재생의 전제(정렬 키가 지금 상태만의 함수)도 깨집니다.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum RankChange {
    #[default]
    None,
    Users(Vec<UserId>),
    /// 역할 등급·소유자 규칙처럼 여러 사람에게 걸치는 변화.
    All,
}

/// 해소 중인 공간 상태와, `SpaceState`에 없는 내부 기록. 멤버·이벤트 수만큼, 또는 끝없이 커지는 기록은 지속 자료
/// 구조(`SharedMap`, `imbl::OrdSet`)라 해소기 스냅숏(복제)이 표 크기에 비례하지 않습니다. 나머지(채널 수로 묶인
/// `legacy_channels`·`history_consent` 등)는 스냅숏마다 복사됩니다.
#[derive(Clone, Debug)]
pub(crate) struct Machine {
    pub space: SpaceId,
    pub genesis: EventId,
    pub st: SpaceState,
    /// 지운 채널·역할 ID는 다시 쓰지 못하게 합니다(옛 초대·덮어쓰기가 새 대상에 붙지 않게). 지울 때마다 늘기만 합니다.
    deleted_channels: imbl::OrdSet<ChannelId>,
    deleted_roles: imbl::OrdSet<RoleId>,
    /// 현재 멤버의 활성 기기 → 사용자. 한 기기가 두 사용자 목록에 동시에 있지 못하게 합니다.
    device_owner: SharedMap<DeviceId, UserId>,
    /// 떠난 사용자의 마지막 기기 기록. 다시 들어올 때 새 목록이 이것과 같거나 이어받아야 합니다(나갔다 들어와 폐기된
    /// 기기를 되살리지 못하게; 판 번호만 보면 폐기를 뺀 새 판으로 되살릴 수 있었음).
    departed: SharedMap<UserId, Arc<MemberDevices>>,
    pub member_since: SharedMap<UserId, u64>,
    /// 크루 전환 요청에 담긴 새 이름.
    crew_name: Option<String>,
    /// 이미 쓴 초대 증명(초대, 만든 시각, 가입자). 상태 레인에서 본 증명을 다른 멤버가 다시 내밀어 나간 사람을 되돌려
    /// 넣지 못하게. 증명은 가입자의 기기 목록에 묶여 있어 이 셋이 같으면 같은 증명입니다. 서명 바이트로 가리지 않는 것은,
    /// 혼합 서명의 Ed25519 쪽은 양자 컴퓨터로 다른 값으로 바꿔 끼울 수 있어서입니다.
    used_proofs: imbl::OrdSet<([u8; 32], u64, UserId)>,
    /// 1:1에서 크루로 바꾼 두 사람과 그때 있던 채널. 이 채널들의 과거 기록 공개는 두 사람이 모두 원해야 켜집니다.
    converted_by: Option<(UserId, UserId)>,
    legacy_channels: BTreeSet<ChannelId>,
    /// 전환 전 채널의 과거 기록 공개에 먼저 동의한 사람(상대의 동의를 기다림).
    history_consent: BTreeMap<ChannelId, UserId>,
    /// 마지막으로 가져간 뒤 역할 순위가 바뀌었을 수 있는 범위(정렬 키 재계산 신호).
    rank_change: RankChange,
    limits: Limits,
    /// 다른 멤버가 전한 복구 판: (대상 사용자, 복구 기기) → 전한 사용자. [`RECOVERY_RELAYS`]명이 전해야 받습니다.
    recovery_relays: SharedMap<(UserId, DeviceId), BTreeSet<UserId>>,
    /// 마지막 체크포인트 순번(없으면 0).
    checkpoint_seq: u64,
    /// 마지막 체크포인트 뒤 적용된 상태 이벤트가 쓴 상태 키. 다음 체크포인트가 유효하다고 적지 않으면 은퇴합니다.
    seen_keys: imbl::OrdSet<StateKeyId>,
    /// 은퇴한 상태 키 → 은퇴시킨 체크포인트 순번. 이 키로 암호화한 상태 이벤트는 거부합니다(체크포인트로 들어온
    /// 멤버는 이 키가 없어 풀 수 없으므로 작성자가 지금 키로 다시 암호화).
    retired_keys: imbl::OrdMap<StateKeyId, u64>,
    /// 마지막 체크포인트 뒤 새로 폐기된 기기(활성이던 기록이 폐기로 바뀜). 체크포인트가 폐기 기록으로 옮깁니다.
    revoked_since: imbl::OrdSet<DeviceId>,
    /// 해시 사슬 폐기 기록의 지금 뿌리.
    revocation_root: [u8; 32],
    /// 마지막 체크포인트 뒤의 소유권 이전 인증서(차례대로).
    owner_certs: Vec<OwnershipCert>,
    /// 지금 유효한 체크포인트 작성 위임(소유권이 바뀌면 사라짐).
    delegation: Option<DelegationCert>,
    /// 지금 바탕 체크포인트에 이견을 낸 기기(기기마다 한 번).
    disputes: imbl::OrdSet<DeviceId>,
    /// 체크포인트가 쓴 초대 증명 기록을 비우며 은퇴시킨 초대 토큰 해시(같은 토큰으로 초대를 다시 만들어 옛 증명을 다시
    /// 쓰지 못하게).
    retired_invites: imbl::OrdSet<[u8; 32]>,
    /// 크루 키 사슬과 표 보유 기기([`keys`]).
    pub(super) keys: CrewKeys,
    /// 지금 적용 중인 이벤트(기기가 활성이 된 이벤트를 기록하려고).
    cur: EventId,
    /// 지금 바탕 체크포인트(정규 인코딩에 들지 않음: 체크포인트 본문에서 다시 만듦).
    pub base: Option<Arc<CheckpointBase>>,
}

/// 받아들인 체크포인트(바탕) 정보. 다음 체크포인트를 검증하고 경계 규칙을 거는 데 씁니다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointBase {
    pub id: CheckpointId,
    pub event: EventId,
    pub seq: u64,
    pub prev: Option<CheckpointId>,
    /// 체크포인트 이벤트의 논리 시계.
    pub lamport: u64,
    /// 경계 F.
    pub floor: u64,
    /// 경계 위 조상(ID 순).
    pub frontier: Arc<Vec<FrontierEntry>>,
    pub author: DeviceId,
    pub state_hash: [u8; 32],
    pub snapshot: BlobId,
    pub snapshot_len: u64,
    /// 체크포인트 이벤트를 암호화한 상태 키.
    pub key: Option<StateKeyId>,
    pub valid_state_keys: Arc<Vec<StateKeyId>>,
    /// 키 그룹 머리(크루는 해소기가 확인한 것, 비공개·덮개는 쓴 기기가 아는 사슬 끝).
    pub key_heads: Arc<Vec<KeyHead>>,
    /// 체크포인트에 실린 소유자 기기 기록.
    pub owner_devices: Arc<MemberDevices>,
    /// 다음 체크포인트를 쓸 수 있는 위임의 해시.
    pub delegation_hash: Option<[u8; 32]>,
    /// 기기 트리 뿌리와 잎 수(다음 체크포인트의 서명 기기 증명 확인).
    pub device_root: [u8; 32],
    pub devices: u64,
}

impl CheckpointBase {
    pub(crate) fn of(
        ev_id: EventId,
        lamport: u64,
        key: Option<StateKeyId>,
        c: &SignedCheckpoint,
    ) -> Self {
        let b = &c.body;
        CheckpointBase {
            id: c.id(),
            event: ev_id,
            seq: b.seq,
            prev: b.prev,
            lamport,
            floor: b.frontier_lamport,
            frontier: Arc::new(b.frontier.clone()),
            author: b.author,
            state_hash: b.state_hash,
            snapshot: b.snapshot,
            snapshot_len: b.snapshot_len,
            key,
            valid_state_keys: Arc::new(b.valid_state_keys.clone()),
            key_heads: Arc::new(b.key_heads.clone()),
            owner_devices: Arc::new(MemberDevices::from_list(&b.owner_devices.list)),
            delegation_hash: b.delegation_hash,
            device_root: b.device_root,
            devices: b.devices,
        }
    }
}

/// 은퇴한 상태 키 기록 상한(넘으면 가장 먼저 은퇴한 것부터 잊음).
const MAX_RETIRED_KEYS: usize = 4096;
/// 정규 인코딩 판. 2판부터 MLS 커밋 기록 대신 크루 키 사슬([`CrewKeys`])을, 3판부터 이모지 한도·올린 사람을 싣습니다.
/// 크루 해산 표시는 판을 올리지 않고 참일 때만 끝에 붙입니다(해산하지 않은 크루는 3판 바이트 그대로).
const CANON_VERSION: u32 = 3;

/// 체크포인트가 정리하며 낸 값(본문에 실을 것).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Sealed {
    pub revoked: Vec<DeviceId>,
    pub revocation_root: [u8; 32],
    pub owner_chain: Vec<OwnershipCert>,
    /// 크루 키 사슬의 머리(아직 갱신이 없으면 `None`). 비공개·덮개 그룹 머리는 해소기가 모르므로 데몬이 더합니다.
    pub crew_head: Option<KeyHead>,
    pub delegation_hash: Option<[u8; 32]>,
}

/// 지금 빌드는 복구 판을 내지도 전하지도 않지만, 지난 상태 이벤트를 다시 적용한 결과가 같도록 규칙은 남습니다.
/// 복구 판(아는 목록에 없던 기기가 자기만 더함)을 상태에 넣으려면 대상 말고 이만큼의 멤버가 따로 전해야 합니다(대상 말고
/// 멤버가 그보다 적으면 모두). 정직한 노드는 저마다 유예 시간이 지난 뒤에만 전하므로, 복구 문구를 훔친 사람과 짠 멤버
/// 한 명이 유예 없이 새 기기를 넣고 그 기기로 살아 있는 기기를 폐기하지 못하게 합니다.
pub(super) const RECOVERY_RELAYS: usize = 2;
/// 사용자마다 모으는 복구 판 수(시드를 가진 쪽이 기기 키를 바꿔 가며 기록을 불리지 못하게).
const MAX_RECOVERY_RELAYS_PER_USER: usize = 8;

impl Machine {
    pub fn create(
        ev: &Stored,
        pin: Option<EventId>,
        limits: Limits,
        memo: &SigMemo,
    ) -> Result<Machine, AuthError> {
        let StoredBody::Full(body) = &ev.body else {
            return Err(AuthError::KindMismatch);
        };
        let EventBody::SpaceCreate {
            kind,
            settings,
            owner,
            committer,
            everyone,
            members,
            devices,
        } = &**body
        else {
            return Err(AuthError::KindMismatch);
        };
        if pin.is_some_and(|g| g != ev.id) {
            return Err(AuthError::NotGenesis);
        }
        if ev.header.user != *owner {
            return Err(AuthError::NotOwner);
        }
        check_space_settings(settings)?;
        let count_ok = match kind {
            SpaceKind::Space => members.len() == 2,
            SpaceKind::SelfSpace => members.len() == 1,
            SpaceKind::Crew => (1..=limits.max_members).contains(&members.len()),
        };
        if !count_ok {
            return Err(AuthError::Invalid("처음 멤버 수"));
        }
        let mut member_map = SharedMap::new();
        for m in members {
            let nickname = normalize_nickname(m.nickname.as_deref())?;
            // 처음에는 @everyone 말고 역할이 없으므로 역할 목록은 비웁니다.
            let clean = Member {
                user: m.user,
                roles: Vec::new(),
                nickname,
                automation: m.automation,
            };
            if member_map.insert(m.user, Arc::new(clean)).is_some() {
                return Err(AuthError::Invalid("멤버 중복"));
            }
        }
        if !member_map.contains_key(owner) {
            return Err(AuthError::Invalid("소유자가 멤버가 아님"));
        }
        if devices.len() > member_map.len() {
            return Err(AuthError::BadDeviceList);
        }
        let mut lists = SharedMap::new();
        let mut device_owner = SharedMap::new();
        let mut added_by = SharedMap::new();
        for sdl in devices {
            check_device_list_shape(sdl)?;
            let u = sdl.list.user;
            if !member_map.contains_key(&u) || lists.contains_key(&u) {
                return Err(AuthError::BadDeviceList);
            }
            for d in sdl.list.active() {
                if device_owner.insert(d.device, u).is_some() {
                    return Err(AuthError::BadDeviceList);
                }
                added_by.insert(d.device, ev.id);
            }
            lists.insert(u, Arc::new(MemberDevices::from_list(&sdl.list)));
        }
        // 서명 검증은 비싸서 구조 검사를 다 통과한 뒤에 한 번만 합니다(결과는 기억).
        if !memo.lists_ok(body) {
            return Err(AuthError::BadDeviceList);
        }
        if !lists
            .get(owner)
            .is_some_and(|l: &Arc<MemberDevices>| l.contains_active(&ev.header.author))
        {
            return Err(AuthError::UnknownDevice);
        }
        if device_owner.get(committer) != Some(owner) {
            return Err(AuthError::InvalidCommitter);
        }
        let everyone_perms = if *kind == SpaceKind::Space {
            FRIEND_SPACE_EVERYONE
        } else {
            *everyone
        };
        let mut roles = BTreeMap::new();
        roles.insert(RoleId::EVERYONE, Role::everyone(everyone_perms));
        let member_since = member_map.keys().map(|u| (*u, ev.header.lamport)).collect();
        let mut keys = CrewKeys::new(*committer);
        keys.added_by = added_by;
        Ok(Machine {
            space: ev.header.space,
            genesis: ev.id,
            st: SpaceState {
                kind: *kind,
                owner: *owner,
                committer: *committer,
                settings: settings.clone(),
                members: member_map,
                roles,
                channels: SharedMap::new(),
                devices: lists,
                invites: Default::default(),
                emojis: BTreeMap::new(),
                pending_crew_conversion: None,
                bans: SharedMap::new(),
                archives: BTreeMap::new(),
                channel_transport: BTreeMap::new(),
                role_display: SharedMap::new(),
                anonymous_channels: Default::default(),
                crew_servers: Default::default(),
                forum_featured: Default::default(),
                attach_limits: Default::default(),
                volatile_caps: Default::default(),
                edit_rules: Default::default(),
                emoji_limits: Default::default(),
                emoji_by: Default::default(),
                disbanded: false,
            },
            deleted_channels: imbl::OrdSet::new(),
            deleted_roles: imbl::OrdSet::new(),
            device_owner,
            departed: SharedMap::new(),
            member_since,
            crew_name: None,
            used_proofs: imbl::OrdSet::new(),
            converted_by: None,
            legacy_channels: BTreeSet::new(),
            history_consent: BTreeMap::new(),
            recovery_relays: SharedMap::new(),
            rank_change: RankChange::None,
            limits,
            keys,
            cur: ev.id,
            checkpoint_seq: 0,
            seen_keys: imbl::OrdSet::new(),
            retired_keys: imbl::OrdMap::new(),
            revoked_since: imbl::OrdSet::new(),
            revocation_root: [0; 32],
            owner_certs: Vec::new(),
            delegation: None,
            disputes: imbl::OrdSet::new(),
            retired_invites: imbl::OrdSet::new(),
            base: None,
        })
    }

    pub fn apply(&mut self, ev: &Stored, memo: &SigMemo, dag: &dyn Dag) -> R {
        let h = &ev.header;
        if h.space != self.space {
            return Err(AuthError::WrongSpace);
        }
        self.cur = ev.id;
        let user = h.user;
        if self.st.disbanded {
            return Err(AuthError::Invalid("해산한 크루"));
        }
        if !self.st.is_member(&user) {
            return Err(AuthError::NotMember);
        }
        if !self.st.device_belongs(&user, &h.author) {
            return Err(AuthError::UnknownDevice);
        }
        let body = match &ev.body {
            StoredBody::Full(b) => &**b,
            StoredBody::Join(f) => {
                let eff = self.member_join(h, f);
                return self.settle(eff);
            }
            StoredBody::Devices(f) => {
                let eff = self.device_list_update(&user, f);
                return self.settle(eff);
            }
            StoredBody::Key(f) => {
                let eff = self.key_update(ev.id, h, f, dag);
                return self.settle(eff);
            }
        };
        let eff = match body {
            EventBody::SpaceCreate { .. } => Err(AuthError::AlreadyCreated),
            EventBody::SpaceUpdate { settings } => self.space_update(&user, settings),
            EventBody::ChannelCreate { channel, settings } => {
                self.channel_create(&user, channel, settings)
            }
            EventBody::ChannelUpdate { channel, settings } => {
                self.channel_update(&user, channel, settings)
            }
            EventBody::ChannelDelete { channel } => self.channel_delete(&user, channel),
            EventBody::ChannelTransportSet { channel, transport } => {
                self.channel_transport_set(&user, channel, *transport)
            }
            EventBody::RoleCreate { role } => self.role_create(&user, role),
            EventBody::RoleUpdate { role } => self.role_update(&user, role),
            EventBody::RoleDelete { role } => self.role_delete(&user, role),
            EventBody::RoleDisplaySet { role, display } => {
                self.role_display_set(&user, role, display)
            }
            EventBody::ChannelAnonymousSet { channel, anonymous } => {
                self.channel_anonymous_set(&user, channel, *anonymous)
            }
            EventBody::CrewServersSet { stun, turn } => self.crew_servers_set(&user, stun, turn),
            EventBody::ForumFeaturedSet { channel, post } => {
                self.forum_featured_set(&user, channel, post.as_ref())
            }
            EventBody::ChannelAttachLimitSet { channel, max_bytes } => {
                self.channel_attach_limit_set(&user, channel, *max_bytes)
            }
            EventBody::ChannelVolatileCapSet { channel, cap } => {
                self.channel_volatile_cap_set(&user, channel, *cap)
            }
            EventBody::EmojiLimitsSet { limits } => self.emoji_limits_set(&user, *limits),
            EventBody::ChannelEditRulesSet {
                channel,
                no_edit,
                no_delete,
            } => self.channel_edit_rules_set(
                &user,
                channel,
                EditRules {
                    no_edit: *no_edit,
                    no_delete: *no_delete,
                },
            ),
            // 해소기는 이 둘을 받을 때 간추리므로(`Stored::new`) 여기 오는 일은 없지만, 오면 같은 판정을 냅니다.
            EventBody::MemberJoin {
                member,
                devices,
                profile,
                invite,
            } => self.member_join(
                h,
                &JoinFacts::of(
                    &h.space,
                    member.clone(),
                    devices,
                    profile.as_ref(),
                    invite.as_ref(),
                ),
            ),
            EventBody::MemberUpdate {
                user: target,
                roles,
                nickname,
            } => self.member_update(&user, target, roles, nickname.as_deref()),
            EventBody::MemberProfile { nickname, profile } => {
                self.member_profile(&user, nickname.as_deref(), profile.as_ref(), || {
                    memo.profile_ok(body)
                })
            }
            // 퍼뜨리기만 하는 카드: 멤버가 자기 카드만 냅니다.
            EventBody::MemberBadges { card } => {
                if !self.st.is_member(&user) {
                    Err(AuthError::NotMember)
                } else if card.user != user || !memo.profile_ok(body) {
                    Err(AuthError::BadProfile)
                } else {
                    Ok(Effect::none())
                }
            }
            EventBody::MemberKick {
                user: target,
                reason,
            } => self.member_kick(&user, target, reason),
            EventBody::MemberLeave { user: target } => self.member_leave(&user, target),
            EventBody::MemberBan {
                user: target,
                reason,
                delete_since,
            } => self.member_ban(h, target, reason, *delete_since),
            EventBody::MemberUnban { user: target } => self.member_unban(&user, target),
            EventBody::ArchiveGrant(g) => self.archive_grant(h, g),
            EventBody::ArchiveRevoke { device } => self.archive_revoke(&user, device),
            EventBody::InviteCreate { invite } => self.invite_create(&user, invite),
            EventBody::InviteRevoke { token_hash } => self.invite_revoke(&user, token_hash),
            EventBody::EmojiAdd { emoji } => self.emoji_add(&user, emoji),
            EventBody::EmojiRemove { id } => self.emoji_remove(&user, id),
            EventBody::CrewConvertRequest { name } => self.crew_request(&user, name),
            EventBody::CrewConvertAccept => self.crew_accept(&user),
            EventBody::OwnershipTransfer { new_owner, cert } => {
                self.ownership_transfer(h, new_owner, cert, || memo.cert_ok(body))
            }
            EventBody::CommitterChange { device } => self.committer_change(&user, device),
            EventBody::DeviceListUpdate { devices } => {
                self.device_list_update(&user, &DevicesFacts::of(devices))
            }
            // 해소기는 받을 때 간추리므로(`Stored::new`) 여기 오는 일은 없지만, 오면 같은 판정을 냅니다.
            EventBody::KeyUpdate(p) => self.key_update(ev.id, h, &KeyFacts::of(p), dag),
            EventBody::KeyBackupSet { device } => self.key_backup_set(&user, device.as_ref()),
            // 게시글 작성자를 모르므로 여기서는 고정·잠금 권한 등 상태만으로 알 수 있는 것만 봅니다.
            // 작성자 확인은 데몬이 `authorize_message(.., Some(작성자))`로 합니다.
            EventBody::ForumPostUpdate { .. } => {
                check_forum_update(&self.st, h, body, None).map(|()| Effect::none())
            }
            EventBody::Message(_)
            | EventBody::Edit { .. }
            | EventBody::Delete { .. }
            | EventBody::Reaction { .. }
            | EventBody::ReactionRemove { .. }
            | EventBody::ForumPost { .. }
            | EventBody::KeyShare(_)
            | EventBody::VolatileRotation(_) => Err(AuthError::WrongLane),
            // 체크포인트는 해소기가 경계로 따로 판정합니다(`state::judge`). 여기까지 오면 바탕이 될 수 없는 것입니다.
            EventBody::StateCheckpoint(c) => Err(self.stale_checkpoint(c)),
            EventBody::CheckpointDelegation(cert) => {
                self.checkpoint_delegation(h, cert, || memo.cert_ok(body))
            }
            EventBody::CheckpointDispute {
                checkpoint,
                seq,
                state_hash: _,
            } => self.checkpoint_dispute(h, checkpoint, *seq),
            // 멤버(위에서 확인)가 자기 표시를 내는 것뿐이라 상태는 그대로입니다. 모으기는 데몬이 합니다.
            EventBody::MemberFlair(f) if f.is_valid() => Ok(Effect::none()),
            EventBody::MemberFlair(_) => Err(AuthError::Invalid("후원자 표시 형식")),
            EventBody::CrewDisband => self.crew_disband(&user),
        };
        self.settle(eff)
    }

    /// 상태를 바꾼 적용 뒤의 정리(확정 기기 고치기, 떠난 멤버의 보관 허가 빼기, 표 보유 기기 자격).
    fn settle(&mut self, eff: R) -> R {
        let eff = eff?;
        if eff.changed {
            self.repair_committer();
            self.drop_member_archives();
            self.check_holders();
        }
        Ok(eff)
    }

    // ---- 공통 검사 ----

    fn need(&self, user: &UserId, p: Permissions) -> Result<(), AuthError> {
        if self.st.base_permissions(user).contains(p) {
            Ok(())
        } else {
            Err(AuthError::MissingPermission(p))
        }
    }

    fn need_in(&self, user: &UserId, channel: &ChannelId, p: Permissions) -> Result<(), AuthError> {
        if self.st.channel_permissions(user, channel).contains(p) {
            Ok(())
        } else {
            Err(AuthError::MissingPermission(p))
        }
    }

    fn need_kind(&self, kind: SpaceKind) -> Result<(), AuthError> {
        if self.st.kind == kind {
            Ok(())
        } else {
            Err(AuthError::WrongSpaceKind)
        }
    }

    /// 1대1 스페이스에는 역할이 없습니다(두 친구가 동등).
    fn not_friend_space(&self) -> Result<(), AuthError> {
        if self.st.kind == SpaceKind::Space {
            Err(AuthError::WrongSpaceKind)
        } else {
            Ok(())
        }
    }

    fn rank(&self, user: &UserId) -> u64 {
        self.st.rank_of(user)
    }

    /// 자기가 가진 권한만 남에게 주거나 뺄 수 있습니다(관리자·소유자는 전부).
    fn may_grant(&self, user: &UserId, bits: Permissions) -> Result<(), AuthError> {
        let base = self.st.base_permissions(user);
        if base == Permissions::ALL || base.contains(bits) {
            Ok(())
        } else {
            Err(AuthError::MissingPermission(bits.remove(base)))
        }
    }

    /// 역할의 위아래 순위(`Role::level`: 관리자 역할은 일반 역할보다 늘 위).
    fn role_rank(&self, id: &RoleId) -> u64 {
        self.st.roles.get(id).map_or(0, Role::level)
    }

    /// 있는 역할만 남기고(@everyone 제외) 정렬·중복 제거합니다. 동시에 지워진 역할 때문에
    /// 이벤트 전체가 거부되지 않도록 모르는 역할은 조용히 뺍니다.
    fn normalize_roles(&self, roles: &[RoleId]) -> Result<Vec<RoleId>, AuthError> {
        if roles.len() > MAX_ROLES {
            return Err(AuthError::Limit("역할 수"));
        }
        let mut out: Vec<RoleId> = roles
            .iter()
            .filter(|r| **r != RoleId::EVERYONE && self.st.roles.contains_key(r))
            .copied()
            .collect();
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// 역할을 주려면 역할 관리 권한과, 주는 역할보다 높은 순위가 필요합니다.
    fn may_assign(&self, user: &UserId, roles: &[RoleId]) -> Result<(), AuthError> {
        if roles.is_empty() {
            return Ok(());
        }
        self.need(user, Permissions::MANAGE_ROLES)?;
        let r = self.rank(user);
        if roles.iter().any(|id| self.role_rank(id) >= r) {
            return Err(AuthError::RankTooLow);
        }
        Ok(())
    }

    /// 기기 목록 검사: 구조, 서명, 주인, 다른 멤버 기기와의 충돌, 최소 버전.
    fn check_member_devices(&self, user: &UserId, list: &ListFacts, min_version: Option<u64>) -> R {
        if !list.shape_ok || !list.sig_ok {
            return Err(AuthError::BadDeviceList);
        }
        if list.user != *user {
            return Err(AuthError::BadDeviceList);
        }
        if min_version.is_some_and(|v| list.version < v) {
            return Err(AuthError::StaleDeviceList);
        }
        for d in list.record.active() {
            if self.device_owner.get(&d.device).is_some_and(|o| o != user) {
                return Err(AuthError::BadDeviceList);
            }
        }
        Ok(Effect::none())
    }

    fn set_device_list(&mut self, user: UserId, list: Arc<MemberDevices>) {
        let mut old = BTreeSet::new();
        if let Some(prev) = self.st.devices.get(&user) {
            for d in prev.active() {
                old.insert(d.device);
                if self.device_owner.get(&d.device) == Some(&user) {
                    self.device_owner.remove(&d.device);
                }
                // 활성이던 기기가 폐기로 바뀌면 다음 체크포인트의 폐기 기록에 싣습니다.
                if list.get(&d.device).is_some_and(|n| n.revoked_at.is_some()) {
                    self.revoked_since.insert(d.device);
                }
            }
        }
        let mut now = BTreeSet::new();
        for d in list.active() {
            now.insert(d.device);
            self.device_owner.insert(d.device, user);
        }
        self.st.devices.insert(user, list);
        // 크루 키 사슬: 빠진 기기는 다음 회전이 빼야 하고, 새 기기는 이 이벤트가 넣은 것으로 기록합니다.
        for d in old.difference(&now) {
            self.lost_device(*d);
        }
        for d in now.difference(&old) {
            self.gained_device(*d);
        }
    }

    fn rank_changed(&mut self, user: &UserId) {
        match &mut self.rank_change {
            RankChange::All => {}
            RankChange::Users(users) => users.push(*user),
            c @ RankChange::None => *c = RankChange::Users(vec![*user]),
        }
    }

    /// 지난번 뒤로 순위가 바뀌었을 수 있는 범위를 가져가고 비웁니다.
    pub fn take_rank_change(&mut self) -> RankChange {
        std::mem::take(&mut self.rank_change)
    }

    fn remove_member(&mut self, user: &UserId) {
        self.st.members.remove(user);
        if let Some(list) = self.st.devices.remove(user) {
            for d in list.active() {
                if self.device_owner.get(&d.device) == Some(user) {
                    self.device_owner.remove(&d.device);
                }
                self.lost_device(d.device);
            }
            self.departed.insert(*user, list);
        }
        self.member_since.remove(user);
        if self.st.pending_crew_conversion == Some(*user) {
            self.st.pending_crew_conversion = None;
            self.crew_name = None;
        }
        self.rank_changed(user);
    }

    /// 확정 기기는 소유자나 ADMINISTRATOR 멤버의 활성 기기여야 합니다. 폐기·탈퇴·강등으로 자격을 잃으면
    /// 결정론적으로 넘깁니다: 소유자의 가장 작은 활성 기기, 소유자에게 활성 기기가 없으면 ADMINISTRATOR 멤버의 활성
    /// 기기 중 가장 작은 것. 둘 다 없으면 그대로 두고 소유자의 조치를 기다립니다.
    fn repair_committer(&mut self) {
        let valid = self.device_owner.get(&self.st.committer).is_some_and(|u| {
            *u == self.st.owner
                || self
                    .st
                    .base_permissions(u)
                    .contains(Permissions::ADMINISTRATOR)
        });
        if valid {
            return;
        }
        let owner_device = self
            .st
            .devices
            .get(&self.st.owner)
            .and_then(|l| l.active().map(|e| e.device).min());
        let admin_device = || {
            self.device_owner
                .iter()
                .filter(|(_, u)| {
                    self.st
                        .base_permissions(u)
                        .contains(Permissions::ADMINISTRATOR)
                })
                .map(|(d, _)| *d)
                .min()
        };
        if let Some(d) = owner_device.or_else(admin_device) {
            self.st.committer = d;
        }
    }

    fn check_channel_settings(
        &self,
        id: &ChannelId,
        s: &ChannelSettings,
        old: Option<&ChannelSettings>,
    ) -> R {
        check_name(&s.name, MAX_NAME_CHARS, "채널 이름")?;
        check_free_text(&s.topic, MAX_TOPIC_CHARS, "채널 주제")?;
        if old.is_some_and(|o| o.kind != s.kind || o.security != s.security) {
            // 보안 등급이 바뀌면 이미 저장된 데이터의 보관 약속이 깨집니다.
            return Err(AuthError::Invalid("채널 종류·보안 등급은 바꿀 수 없음"));
        }
        if let Some(p) = &s.parent {
            let parent_ok = p != id
                && s.kind != ChannelKind::Category
                && self
                    .st
                    .channels
                    .get(p)
                    .is_some_and(|c| c.kind == ChannelKind::Category);
            if !parent_ok {
                return Err(AuthError::Invalid("상위 채널"));
            }
        }
        if s.ttl == ChatTtl::Seconds(0) {
            return Err(AuthError::Invalid("채팅 수명"));
        }
        if s.overwrites.len() > MAX_OVERWRITES {
            return Err(AuthError::Limit("권한 덮어쓰기 수"));
        }
        for (i, o) in s.overwrites.iter().enumerate() {
            if s.overwrites[..i].iter().any(|x| x.target == o.target) {
                return Err(AuthError::Invalid("권한 덮어쓰기 대상 중복"));
            }
            if let OverwriteTarget::Role(r) = &o.target
                && !self.st.roles.contains_key(r)
            {
                return Err(AuthError::UnknownRole);
            }
        }
        if s.forum_tags.len() > MAX_FORUM_TAGS {
            return Err(AuthError::Limit("포럼 태그 수"));
        }
        for (i, t) in s.forum_tags.iter().enumerate() {
            if s.forum_tags[..i].iter().any(|x| x.id == t.id) {
                return Err(AuthError::Invalid("포럼 태그 중복"));
            }
            check_name(&t.name, MAX_NAME_CHARS, "포럼 태그 이름")?;
            if let Some(e) = &t.emoji {
                check_free_text(e, MAX_NAME_CHARS, "포럼 태그 이모지")?;
            }
        }
        Ok(Effect::none())
    }

    // ---- 종류별 처리 ----

    fn space_update(&mut self, user: &UserId, settings: &SpaceSettings) -> R {
        self.need(user, Permissions::MANAGE_SPACE)?;
        check_space_settings(settings)?;
        self.st.settings = settings.clone();
        Ok(Effect::changed())
    }

    fn channel_create(&mut self, user: &UserId, id: &ChannelId, s: &ChannelSettings) -> R {
        self.need(user, Permissions::MANAGE_CHANNELS)?;
        if self.st.channels.contains_key(id) || self.deleted_channels.contains(id) {
            return Err(AuthError::ChannelExists);
        }
        if self.st.channels.len() >= MAX_CHANNELS {
            return Err(AuthError::Limit("채널 수"));
        }
        self.check_channel_settings(id, s, None)?;
        self.may_grant(user, overwrite_delta(&[], &s.overwrites))?;
        self.st.channels.insert(*id, s.clone());
        Ok(Effect::changed())
    }

    fn channel_update(&mut self, user: &UserId, id: &ChannelId, s: &ChannelSettings) -> R {
        let old = self.st.channels.get(id).ok_or(AuthError::UnknownChannel)?;
        self.need_in(user, id, Permissions::MANAGE_CHANNELS)?;
        self.check_channel_settings(id, s, Some(old))?;
        self.may_grant(user, overwrite_delta(&old.overwrites, &s.overwrites))?;
        let old_history = old.history;
        let mut s = s.clone();
        if self.legacy_channels.contains(id) {
            match self.legacy_history(user, id, old_history, s.history)? {
                Consent::Apply => {}
                Consent::Wait => s.history = old_history,
            }
        }
        self.st.channels.insert(*id, s);
        Ok(Effect::changed())
    }

    /// 1:1에서 크루로 바꾸기 전부터 있던 채널의 과거 기록 공개(기획서: 두 사람이 합의해 바꿈). 비공개 → 공개는 원래
    /// 두 사람 중 하나가 먼저 바꾸면 동의로만 기록하고, 다른 한 사람도 바꾸면 그때 켭니다. 다른 멤버는 켤 수 없습니다.
    /// 공개로 바꾸지 않는 변경은 그 사람의 동의를 거둡니다.
    fn legacy_history(
        &mut self,
        user: &UserId,
        id: &ChannelId,
        old: HistoryVisibility,
        new: HistoryVisibility,
    ) -> Result<Consent, AuthError> {
        let turning_public = old != HistoryVisibility::Public && new == HistoryVisibility::Public;
        if !turning_public {
            if self.history_consent.get(id) == Some(user) {
                self.history_consent.remove(id);
            }
            return Ok(Consent::Apply);
        }
        let other = match self.converted_by {
            Some((x, y)) if x == *user => y,
            Some((x, y)) if y == *user => x,
            _ => {
                return Err(AuthError::Invalid(
                    "전환 전 채널의 과거 기록 공개는 원래 두 사람만 바꿀 수 있음",
                ));
            }
        };
        if self.history_consent.get(id) == Some(&other) {
            self.history_consent.remove(id);
            Ok(Consent::Apply)
        } else {
            self.history_consent.insert(*id, *user);
            Ok(Consent::Wait)
        }
    }

    fn channel_delete(&mut self, user: &UserId, id: &ChannelId) -> R {
        if !self.st.channels.contains_key(id) {
            return Err(AuthError::UnknownChannel);
        }
        self.need_in(user, id, Permissions::MANAGE_CHANNELS)?;
        self.st.channels.remove(id);
        self.st.channel_transport.remove(id);
        self.st.anonymous_channels.remove(id);
        self.st.forum_featured.remove(id);
        self.st.attach_limits.remove(id);
        self.st.volatile_caps.remove(id);
        self.st.edit_rules.remove(id);
        update_where(
            &mut self.st.channels,
            |c| c.parent == Some(*id),
            |c| c.parent = None,
        );
        self.legacy_channels.remove(id);
        self.history_consent.remove(id);
        self.deleted_channels.insert(*id);
        Ok(Effect::changed())
    }

    /// 채널 내용 전송 방식 바꾸기(양쪽으로). 지금 값과 같으면 거부해 아무것도 바꾸지 않는 이벤트가 쌓이지 않게 합니다.
    fn channel_transport_set(
        &mut self,
        user: &UserId,
        id: &ChannelId,
        transport: ChannelTransport,
    ) -> R {
        if self.st.kind == SpaceKind::SelfSpace {
            // 내 기기끼리만 쓰는 공간이라 IP를 드러낼 상대가 없습니다.
            return Err(AuthError::WrongSpaceKind);
        }
        let ch = self.st.channels.get(id).ok_or(AuthError::UnknownChannel)?;
        self.need_in(user, id, Permissions::MANAGE_CHANNELS)?;
        if !matches!(ch.kind, ChannelKind::Text | ChannelKind::Voice) {
            return Err(AuthError::Invalid("P2P 전용은 텍스트·통화 채널만"));
        }
        let current = self
            .st
            .channel_transport
            .get(id)
            .copied()
            .unwrap_or_default();
        if transport == current {
            return Err(AuthError::Invalid("이미 그 전송 방식"));
        }
        // Tor는 기본값이라 표에 두지 않습니다(`is_p2p`가 표만 봄).
        match transport {
            ChannelTransport::Tor => self.st.channel_transport.remove(id),
            ChannelTransport::P2p => self.st.channel_transport.insert(*id, transport),
        };
        Ok(Effect::changed())
    }

    /// 채널 익명 켜기·끄기. 크루에서만(1대1·내 기기끼리는 누가 썼는지 뻔함), 카테고리는 안 되고, 지금 값과 같으면 거부합니다.
    fn channel_anonymous_set(&mut self, user: &UserId, id: &ChannelId, anonymous: bool) -> R {
        if self.st.kind != SpaceKind::Crew {
            return Err(AuthError::WrongSpaceKind);
        }
        let ch = self.st.channels.get(id).ok_or(AuthError::UnknownChannel)?;
        self.need_in(user, id, Permissions::MANAGE_CHANNELS)?;
        if ch.kind == ChannelKind::Category {
            return Err(AuthError::Invalid("카테고리는 익명으로 만들 수 없음"));
        }
        if anonymous == self.st.is_anonymous(id) {
            return Err(AuthError::Invalid("이미 그 익명 설정"));
        }
        if anonymous {
            self.st.anonymous_channels.insert(*id);
        } else {
            self.st.anonymous_channels.remove(id);
        }
        Ok(Effect::changed())
    }

    /// 포럼 대문 게시글 정하기. 그 채널의 `MANAGE_CHANNELS`, 포럼 채널만, 지금 값과 달라야 합니다. 게시글이 있는지는
    /// 메시지 레인이라 여기서 알 수 없어 ID만 둡니다.
    fn forum_featured_set(&mut self, user: &UserId, id: &ChannelId, post: Option<&EventId>) -> R {
        let ch = self.st.channels.get(id).ok_or(AuthError::UnknownChannel)?;
        self.need_in(user, id, Permissions::MANAGE_CHANNELS)?;
        if ch.kind != ChannelKind::Forum {
            return Err(AuthError::Invalid("포럼 채널이 아님"));
        }
        if self.st.forum_featured.get(id) == post {
            return Err(AuthError::Invalid("이미 그 대문"));
        }
        match post {
            Some(p) => self.st.forum_featured.insert(*id, *p),
            None => self.st.forum_featured.remove(id),
        };
        Ok(Effect::changed())
    }

    /// 채널 첨부 한도 정하기(`None`이면 무제한, 0이면 첨부 금지). 크루에서만, 그 채널의 `MANAGE_CHANNELS`, 카테고리는
    /// 안 되고, 0 또는 [`MIN_ATTACH_LIMIT`] 이상, 지금 값과 달라야 합니다.
    fn channel_attach_limit_set(
        &mut self,
        user: &UserId,
        id: &ChannelId,
        max_bytes: Option<u64>,
    ) -> R {
        if self.st.kind != SpaceKind::Crew {
            return Err(AuthError::WrongSpaceKind);
        }
        let ch = self.st.channels.get(id).ok_or(AuthError::UnknownChannel)?;
        self.need_in(user, id, Permissions::MANAGE_CHANNELS)?;
        if ch.kind == ChannelKind::Category {
            return Err(AuthError::Invalid("카테고리에는 첨부 한도가 없음"));
        }
        if max_bytes.is_some_and(|n| !valid_attach_limit(n)) {
            return Err(AuthError::Invalid("첨부 한도가 너무 작음"));
        }
        if self.st.attach_limits.get(id).copied() == max_bytes {
            return Err(AuthError::Invalid("이미 그 첨부 한도"));
        }
        match max_bytes {
            Some(n) => self.st.attach_limits.insert(*id, n),
            None => self.st.attach_limits.remove(id),
        };
        Ok(Effect::changed())
    }

    /// 상시 휘발성 채널 참가 기기 상한 정하기(`None`이면 기본값). 크루에서만, 그 채널의 `MANAGE_CHANNELS`, 휘발성
    /// 채널만, 범위 안이고 지금 값과 달라야 합니다.
    fn channel_volatile_cap_set(&mut self, user: &UserId, id: &ChannelId, cap: Option<u32>) -> R {
        if self.st.kind != SpaceKind::Crew {
            return Err(AuthError::WrongSpaceKind);
        }
        let ch = self.st.channels.get(id).ok_or(AuthError::UnknownChannel)?;
        self.need_in(user, id, Permissions::MANAGE_CHANNELS)?;
        if !ch.is_volatile() {
            return Err(AuthError::Invalid("휘발성 채널이 아님"));
        }
        if cap.is_some_and(|n| !(MIN_VOLATILE_CAP..=MAX_VOLATILE_CAP).contains(&n)) {
            return Err(AuthError::Invalid("참가 상한 범위 밖"));
        }
        if self.st.volatile_caps.get(id).copied() == cap {
            return Err(AuthError::Invalid("이미 그 참가 상한"));
        }
        match cap {
            Some(n) => self.st.volatile_caps.insert(*id, n),
            None => self.st.volatile_caps.remove(id),
        };
        Ok(Effect::changed())
    }

    /// 채널 수정·삭제 금지 정하기. 크루에서만, 그 채널의 `MANAGE_CHANNELS`, 카테고리는 안 되고, 지금 값과 달라야 합니다.
    fn channel_edit_rules_set(&mut self, user: &UserId, id: &ChannelId, rules: EditRules) -> R {
        if self.st.kind != SpaceKind::Crew {
            return Err(AuthError::WrongSpaceKind);
        }
        let ch = self.st.channels.get(id).ok_or(AuthError::UnknownChannel)?;
        self.need_in(user, id, Permissions::MANAGE_CHANNELS)?;
        if ch.kind == ChannelKind::Category {
            return Err(AuthError::Invalid("카테고리에는 수정·삭제 금지가 없음"));
        }
        if self.st.edit_rules(id) == rules {
            return Err(AuthError::Invalid("이미 그 수정·삭제 금지"));
        }
        if rules.is_empty() {
            self.st.edit_rules.remove(id);
        } else {
            self.st.edit_rules.insert(*id, rules);
        }
        Ok(Effect::changed())
    }

    /// 크루 STUN·TURN 서버 목록 바꾸기(목록 전체). 크루에서만, `MANAGE_SPACE`, 줄 수·길이 한도 안, 지금과 달라야 합니다.
    fn crew_servers_set(&mut self, user: &UserId, stun: &[String], turn: &[String]) -> R {
        if self.st.kind != SpaceKind::Crew {
            return Err(AuthError::WrongSpaceKind);
        }
        self.need(user, Permissions::MANAGE_SPACE)?;
        let fits = |list: &[String]| {
            list.len() <= MAX_CREW_SERVERS
                && list
                    .iter()
                    .all(|s| !s.is_empty() && s.len() <= MAX_CREW_SERVER_LEN)
        };
        if !fits(stun) || !fits(turn) {
            return Err(AuthError::Invalid("크루 서버 목록이 너무 큼"));
        }
        let next = CrewServers {
            stun: stun.to_vec(),
            turn: turn.to_vec(),
        };
        if next == self.st.crew_servers {
            return Err(AuthError::Invalid("이미 그 서버 목록"));
        }
        self.st.crew_servers = next;
        Ok(Effect::changed())
    }

    fn role_create(&mut self, user: &UserId, role: &Role) -> R {
        self.not_friend_space()?;
        self.need(user, Permissions::MANAGE_ROLES)?;
        if role.id == RoleId::EVERYONE {
            return Err(AuthError::EveryoneProtected);
        }
        if self.st.roles.contains_key(&role.id) || self.deleted_roles.contains(&role.id) {
            return Err(AuthError::RoleExists);
        }
        if self.st.roles.len() > MAX_ROLES {
            return Err(AuthError::Limit("역할 수"));
        }
        check_role(role)?;
        if self.rank(user) <= role.level() {
            return Err(AuthError::RankTooLow);
        }
        self.may_grant(user, role.permissions)?;
        // 순위 변화 없음: 멤버의 역할 목록에는 있는 역할만 들어가고(`normalize_roles`, 지운 ID는 다시 못 씀) 새 역할을
        // 가진 멤버는 아직 없습니다.
        self.st.roles.insert(role.id, role.clone());
        Ok(Effect::changed())
    }

    fn role_update(&mut self, user: &UserId, role: &Role) -> R {
        self.not_friend_space()?;
        self.need(user, Permissions::MANAGE_ROLES)?;
        let old = self.st.roles.get(&role.id).ok_or(AuthError::UnknownRole)?;
        if role.id == RoleId::EVERYONE {
            if role.rank != 0 || role.name != old.name {
                return Err(AuthError::EveryoneProtected);
            }
        } else {
            check_role(role)?;
        }
        let r = self.rank(user);
        if r <= old.level() || r <= role.level() {
            return Err(AuthError::RankTooLow);
        }
        self.may_grant(user, Permissions(old.permissions.0 ^ role.permissions.0))?;
        if old.level() != role.level() {
            self.rank_change = RankChange::All;
        }
        self.st.roles.insert(role.id, role.clone());
        Ok(Effect::changed())
    }

    /// 역할 표시 설정. 화면에서만 거르는 값이라 권한 계산에는 쓰지 않습니다. 동시에 지워진 역할은 조용히 뺍니다.
    fn role_display_set(&mut self, user: &UserId, id: &RoleId, d: &RoleDisplay) -> R {
        self.not_friend_space()?;
        self.need(user, Permissions::MANAGE_ROLES)?;
        let old = self.st.roles.get(id).ok_or(AuthError::UnknownRole)?;
        if self.rank(user) <= old.level() {
            return Err(AuthError::RankTooLow);
        }
        let visible_to = match &d.visible_to {
            None => None,
            // 모든 멤버가 @everyone을 가지므로 @everyone을 고른 것은 '모두'와 같습니다.
            Some(list) if list.contains(&RoleId::EVERYONE) => None,
            Some(list) => Some(self.normalize_roles(list)?),
        };
        if *id == RoleId::EVERYONE && visible_to.is_some() {
            return Err(AuthError::EveryoneProtected);
        }
        let d = RoleDisplay {
            visible_to,
            see_admins: d.see_admins,
        };
        if d == RoleDisplay::default_for(id) {
            self.st.role_display.remove(id);
        } else {
            self.st.role_display.insert(*id, d);
        }
        Ok(Effect::changed())
    }

    fn role_delete(&mut self, user: &UserId, id: &RoleId) -> R {
        self.not_friend_space()?;
        self.need(user, Permissions::MANAGE_ROLES)?;
        if *id == RoleId::EVERYONE {
            return Err(AuthError::EveryoneProtected);
        }
        let old = self.st.roles.get(id).ok_or(AuthError::UnknownRole)?;
        if self.rank(user) <= old.level() {
            return Err(AuthError::RankTooLow);
        }
        self.st.roles.remove(id);
        self.deleted_roles.insert(*id);
        // 그 역할이 걸린 값만 고쳐 써서, 해소기 스냅숏과 나눠 쓰는 나머지 마디는 복사하지 않습니다.
        update_where(
            &mut self.st.members,
            |m| m.roles.contains(id),
            |m| Arc::make_mut(m).roles.retain(|r| r != id),
        );
        let target = OverwriteTarget::Role(*id);
        update_where(
            &mut self.st.channels,
            |c| c.overwrites.iter().any(|o| o.target == target),
            |c| c.overwrites.retain(|o| o.target != target),
        );
        for i in 0..self.st.invites.len() {
            if self.st.invites[i].info.roles.contains(id) {
                self.st.invites[i].info.roles.retain(|r| r != id);
            }
        }
        self.st.role_display.remove(id);
        update_where(
            &mut self.st.role_display,
            |d| d.visible_to.as_ref().is_some_and(|l| l.contains(id)),
            |d| {
                if let Some(list) = &mut d.visible_to {
                    list.retain(|r| r != id);
                }
            },
        );
        self.rank_change = RankChange::All;
        Ok(Effect::changed())
    }

    fn member_join(&mut self, h: &EventHeader, f: &JoinFacts) -> R {
        let member = &f.member;
        let user = h.user;
        // 1대1·본인 공간에 사람을 더하려면 먼저 크루로 전환해야 합니다.
        self.need_kind(SpaceKind::Crew)?;
        let target = member.user;
        if self.st.is_member(&target) {
            return Err(AuthError::AlreadyMember);
        }
        // 초대가 있든 관리자가 직접 넣든, 밴을 먼저 풀어야 다시 들어올 수 있습니다.
        if self.st.bans.contains_key(&target) {
            return Err(AuthError::Banned);
        }
        if self.st.members.len() >= self.limits.max_members {
            return Err(AuthError::Limit("멤버 수"));
        }
        self.check_member_devices(&target, &f.list, None)?;
        // 다시 들어오는 사람은 나갈 때의 기록과 같거나 그것을 이어받는 목록이어야 합니다(폐기를 되살리지 못하게).
        if let Some(old) = self.departed.get(&target)
            && *f.list.record != **old
            && !f.list.record.extends(old)
        {
            return Err(AuthError::StaleDeviceList);
        }
        if !f.profile_ok {
            return Err(AuthError::BadProfile);
        }
        let nickname = normalize_nickname(member.nickname.as_deref())?;

        let (invite_idx, roles) = match &f.invite {
            Some(proof) => {
                let idx = self
                    .st
                    .invites
                    .iter()
                    .position(|i| i.info.token_hash == proof.token_hash)
                    .ok_or(AuthError::InvalidInvite)?;
                if !self.st.invites[idx].usable_at(h.created_at) {
                    return Err(AuthError::InvalidInvite);
                }
                // 토큰을 가진 가입자가 이 사람·이 기기 목록으로 만든 증명이어야 하고, 한 번만 쓸 수 있습니다.
                if self
                    .used_proofs
                    .contains(&(proof.token_hash, proof.at.0, target))
                {
                    return Err(AuthError::InvalidInvite);
                }
                if !proof.verifies_with(&self.st.invites[idx].info.verify_key) {
                    return Err(AuthError::InvalidInvite);
                }
                // 초대가 주는 역할은 초대를 만들 때 검사했습니다. 그 사이 지워진 역할만 뺍니다.
                (
                    Some(idx),
                    self.normalize_roles(&self.st.invites[idx].info.roles)?,
                )
            }
            None => {
                self.need(&user, Permissions::MANAGE_SPACE)?;
                let roles = self.normalize_roles(&member.roles)?;
                self.may_assign(&user, &roles)?;
                (None, roles)
            }
        };

        if let Some(i) = invite_idx {
            let inv = &mut self.st.invites[i];
            inv.uses = inv.uses.saturating_add(1);
        }
        if let Some(p) = &f.invite {
            self.used_proofs.insert((p.token_hash, p.at.0, target));
        }
        self.st.members.insert(
            target,
            Arc::new(Member {
                user: target,
                roles,
                nickname,
                automation: member.automation,
            }),
        );
        self.set_device_list(target, f.list.record.clone());
        self.member_since.insert(target, h.lamport);
        // 지속 맵은 없는 키를 지워도 나눠 쓰던 경로를 복사하므로 있을 때만 지웁니다.
        if self.departed.contains_key(&target) {
            self.departed.remove(&target);
        }
        self.rank_changed(&target);
        Ok(Effect::changed())
    }

    fn member_update(
        &mut self,
        user: &UserId,
        target: &UserId,
        roles: &[RoleId],
        nickname: Option<&str>,
    ) -> R {
        let cur = self
            .st
            .members
            .get(target)
            .ok_or(AuthError::UnknownMember)?;
        let new_roles = self.normalize_roles(roles)?;
        let mut old_roles = cur.roles.clone();
        old_roles.sort_unstable();
        old_roles.dedup();
        let roles_changed = old_roles != new_roles;
        if roles_changed {
            self.not_friend_space()?;
            self.need(user, Permissions::MANAGE_ROLES)?;
            let r = self.rank(user);
            let changed = old_roles
                .iter()
                .filter(|x| !new_roles.contains(x))
                .chain(new_roles.iter().filter(|x| !old_roles.contains(x)));
            for id in changed {
                if self.role_rank(id) >= r {
                    return Err(AuthError::RankTooLow);
                }
            }
            if user != target && self.rank(target) >= r {
                return Err(AuthError::RankTooLow);
            }
        }
        // None = 그대로, 빈 문자열 = 지움.
        let new_nick = match nickname {
            None => cur.nickname.clone(),
            Some(n) => normalize_nickname(Some(n))?,
        };
        let nick_changed = new_nick != cur.nickname;
        if nick_changed && user != target {
            // 남의 별명은 역할 관리 권한이 있고 순위가 더 높을 때만.
            self.need(user, Permissions::MANAGE_ROLES)?;
            if self.rank(target) >= self.rank(user) {
                return Err(AuthError::RankTooLow);
            }
        }
        if let Some(m) = self.st.members.get_mut(target) {
            let m = Arc::make_mut(m);
            m.roles = new_roles;
            m.nickname = new_nick;
        }
        if roles_changed {
            self.rank_changed(target);
        }
        Ok(if roles_changed || nick_changed {
            Effect::changed()
        } else {
            Effect::none()
        })
    }

    /// 작성자 자신의 별명·전역 프로필. 역할은 건드리지 않으므로 동시에 바뀐 역할과 부딪히지 않습니다. 별명은 크루에서만,
    /// 프로필은 작성자 것이고 서명이 맞아야 합니다(프로필만 실은 이벤트는 상태를 바꾸지 않음).
    fn member_profile(
        &mut self,
        user: &UserId,
        nickname: Option<&str>,
        profile: Option<&SignedProfile>,
        sig_ok: impl FnOnce() -> bool,
    ) -> R {
        let cur = self.st.members.get(user).ok_or(AuthError::NotMember)?;
        if nickname.is_none() && profile.is_none() {
            return Err(AuthError::Invalid("바꿀 것이 없음"));
        }
        if let Some(p) = profile
            && (p.user != *user || !sig_ok())
        {
            return Err(AuthError::BadProfile);
        }
        let Some(n) = nickname else {
            return Ok(Effect::none());
        };
        self.need_kind(SpaceKind::Crew)?;
        let new_nick = normalize_nickname(Some(n))?;
        if new_nick == cur.nickname {
            return Ok(Effect::none());
        }
        if let Some(m) = self.st.members.get_mut(user) {
            Arc::make_mut(m).nickname = new_nick;
        }
        Ok(Effect::changed())
    }

    fn member_kick(&mut self, user: &UserId, target: &UserId, reason: &str) -> R {
        self.need_kind(SpaceKind::Crew)?;
        self.need(user, Permissions::KICK_MEMBERS)?;
        check_free_text(reason, MAX_REASON_CHARS, "추방 사유")?;
        if !self.st.is_member(target) {
            return Err(AuthError::UnknownMember);
        }
        if *target == self.st.owner {
            return Err(AuthError::OwnerProtected);
        }
        if self.rank(user) <= self.rank(target) {
            return Err(AuthError::RankTooLow);
        }
        self.remove_member(target);
        Ok(Effect::changed())
    }

    /// 멤버가 아니어도(이미 나간 사람 등) 미리 밴할 수 있습니다. 순위 규칙은 추방과 같습니다.
    fn member_ban(
        &mut self,
        h: &EventHeader,
        target: &UserId,
        reason: &str,
        delete_since: Option<yangpa_core::time::Timestamp>,
    ) -> R {
        let user = h.user;
        self.need_kind(SpaceKind::Crew)?;
        self.need(&user, Permissions::BAN_MEMBERS)?;
        check_free_text(reason, MAX_REASON_CHARS, "밴 사유")?;
        if *target == self.st.owner {
            return Err(AuthError::OwnerProtected);
        }
        if self.rank(&user) <= self.rank(target) {
            return Err(AuthError::RankTooLow);
        }
        if self.st.bans.contains_key(target) {
            return Err(AuthError::Banned);
        }
        if self.st.bans.len() >= MAX_BANS {
            return Err(AuthError::Limit("밴 수"));
        }
        // 지울 구간은 밴 시각 앞의 정해진 길이까지만(오래된 기록 전체를 지우는 데 쓰지 못하게).
        if let Some(since) = delete_since {
            let at = h.created_at.as_millis();
            if since.as_millis() > at || at - since.as_millis() > MAX_BAN_DELETE_WINDOW_MS {
                return Err(AuthError::Invalid("최근 메시지 삭제 구간"));
            }
        }
        if self.st.is_member(target) {
            self.remove_member(target);
        }
        self.st.bans.insert(
            *target,
            BanInfo {
                reason: reason.to_owned(),
                banned_by: user,
                banned_at: h.created_at,
            },
        );
        Ok(Effect::changed())
    }

    fn member_unban(&mut self, user: &UserId, target: &UserId) -> R {
        self.need_kind(SpaceKind::Crew)?;
        self.need(user, Permissions::BAN_MEMBERS)?;
        if !self.st.bans.contains_key(target) {
            return Err(AuthError::NotBanned);
        }
        self.st.bans.remove(target);
        Ok(Effect::changed())
    }

    // ---- 보관 전용 노드 ----

    /// 보관 노드 지정(같은 기기면 바꿈). 보관 노드는 멤버가 아니어야 하고, 비공개 채널은 지정하는 사람이 볼 수 있는
    /// 것만 넣을 수 있습니다.
    fn archive_grant(&mut self, h: &EventHeader, g: &ArchiveGrant) -> R {
        let user = h.user;
        self.need_kind(SpaceKind::Crew)?;
        self.need(&user, Permissions::MANAGE_SPACE)?;
        if !g.name.is_empty() {
            check_name(&g.name, archive::MAX_NAME_CHARS, "보관 노드 이름")?;
        }
        check_archive_contact(&g.contact)?;
        if self.st.is_member(&g.user) || self.device_owner.contains_key(&g.device) {
            return Err(AuthError::Invalid("보관 노드가 멤버임"));
        }
        if self.st.bans.contains_key(&g.user) {
            return Err(AuthError::Banned);
        }
        if g.private_channels.len() > archive::MAX_PRIVATE_CHANNELS {
            return Err(AuthError::Limit("보관할 비공개 채널 수"));
        }
        let mut seen = BTreeSet::new();
        for c in &g.private_channels {
            let ch = self.st.channels.get(c).ok_or(AuthError::UnknownChannel)?;
            if !seen.insert(*c)
                || !ch.is_private()
                || ch.is_volatile()
                || ch.kind == ChannelKind::Category
            {
                return Err(AuthError::Invalid("보관할 수 없는 비공개 채널"));
            }
            self.need_in(&user, c, Permissions::VIEW_CHANNEL)?;
        }
        if !self.st.archives.contains_key(&g.device)
            && self.st.archives.len() >= archive::MAX_ARCHIVES
        {
            return Err(AuthError::Limit("보관 노드 수"));
        }
        self.st.archives.insert(
            g.device,
            ArchiveEntry {
                grant: g.clone(),
                granted_by: user,
                granted_at: h.created_at,
            },
        );
        Ok(Effect::changed())
    }

    fn archive_revoke(&mut self, user: &UserId, device: &DeviceId) -> R {
        self.need_kind(SpaceKind::Crew)?;
        self.need(user, Permissions::MANAGE_SPACE)?;
        if self.st.archives.remove(device).is_none() {
            return Err(AuthError::Invalid("지정되지 않은 보관 노드"));
        }
        Ok(Effect::changed())
    }

    /// 보관 노드의 사용자·기기가 멤버가 되면 지정을 지웁니다(멤버는 키를 받으므로 보관 노드일 수 없음).
    fn drop_member_archives(&mut self) {
        if self.st.archives.is_empty() {
            return;
        }
        let members = &self.st.members;
        let devices = &self.device_owner;
        self.st
            .archives
            .retain(|d, e| !members.contains_key(&e.grant.user) && !devices.contains_key(d));
    }

    fn member_leave(&mut self, user: &UserId, target: &UserId) -> R {
        if user != target {
            return Err(AuthError::Invalid("본인만 탈퇴할 수 있음"));
        }
        if *user == self.st.owner {
            // 1:1 공간에서 나가기(친구 삭제)는 소유자도 할 수 있고, 남은 사람이 소유자가 됩니다(확정 기기도
            // `repair_committer`가 넘김). 크루 소유자는 먼저 소유권을 넘겨야 합니다.
            let other = (self.st.kind == SpaceKind::Space)
                .then(|| self.st.members.keys().find(|u| *u != user).copied())
                .flatten()
                .ok_or(AuthError::OwnerProtected)?;
            self.st.owner = other;
            // 남은 사람이 소유자 순위가 됩니다(나가는 사람은 `remove_member`가 남김).
            self.rank_changed(&other);
        }
        self.remove_member(user);
        Ok(Effect::changed())
    }

    fn invite_create(&mut self, user: &UserId, info: &InviteInfo) -> R {
        self.need_kind(SpaceKind::Crew)?;
        self.need(user, Permissions::CREATE_INVITE)?;
        if self.st.invites.len() >= MAX_INVITES {
            return Err(AuthError::Limit("초대 수"));
        }
        if self
            .st
            .invites
            .iter()
            .any(|i| i.info.token_hash == info.token_hash)
            || self.retired_invites.contains(&info.token_hash)
        {
            return Err(AuthError::InviteExists);
        }
        if info.max_uses == Some(0) {
            return Err(AuthError::Invalid("초대 사용 횟수"));
        }
        let roles = self.normalize_roles(&info.roles)?;
        self.may_assign(user, &roles)?;
        self.st.invites.push_back(InviteState {
            info: InviteInfo {
                roles,
                ..info.clone()
            },
            created_by: *user,
            uses: 0,
        });
        Ok(Effect::changed())
    }

    fn invite_revoke(&mut self, user: &UserId, hash: &[u8; 32]) -> R {
        let idx = self
            .st
            .invites
            .iter()
            .position(|i| i.info.token_hash == *hash)
            .ok_or(AuthError::UnknownInvite)?;
        if self.st.invites[idx].created_by != *user {
            self.need(user, Permissions::CREATE_INVITE)?;
        }
        self.st.invites.remove(idx);
        Ok(Effect::changed())
    }

    fn emoji_add(&mut self, user: &UserId, e: &CustomEmoji) -> R {
        self.need(user, Permissions::MANAGE_EMOJI)?;
        if !emoji::valid_name(&e.name) {
            return Err(AuthError::Invalid("이모지 이름"));
        }
        if !emoji::ALLOWED_MIME.contains(&e.mime.as_str()) {
            return Err(AuthError::Invalid("이모지 형식"));
        }
        if self.st.emojis.contains_key(&e.id) {
            return Err(AuthError::EmojiExists);
        }
        if self.st.emojis.len() >= self.st.emoji_count_limit() {
            return Err(AuthError::Limit("이모지 수"));
        }
        self.st.emojis.insert(e.id, e.clone());
        self.st.emoji_by.insert(e.id, *user);
        Ok(Effect::changed())
    }

    fn emoji_remove(&mut self, user: &UserId, id: &EmojiId) -> R {
        self.need(user, Permissions::MANAGE_EMOJI)?;
        if self.st.emojis.remove(id).is_none() {
            return Err(AuthError::UnknownEmoji);
        }
        self.st.emoji_by.remove(id);
        Ok(Effect::changed())
    }

    /// 크루 이모지 한도 정하기. 크루에서만, `MANAGE_SPACE`(한도를 받는 이모지 관리자가 스스로 풀지 못하게), 개수는
    /// 1..=[`MAX_EMOJIS`], 크기는 1바이트 이상, 지금과 달라야 합니다. 낮춰도 이미 있는 이모지는 그대로 둡니다.
    fn emoji_limits_set(&mut self, user: &UserId, limits: EmojiLimits) -> R {
        self.need_kind(SpaceKind::Crew)?;
        self.need(user, Permissions::MANAGE_SPACE)?;
        if limits
            .max_count
            .is_some_and(|n| n == 0 || n as usize > MAX_EMOJIS)
            || limits.max_bytes == Some(0)
        {
            return Err(AuthError::Invalid("이모지 한도"));
        }
        if self.st.emoji_limits == limits {
            return Err(AuthError::Invalid("이미 그 이모지 한도"));
        }
        self.st.emoji_limits = limits;
        Ok(Effect::changed())
    }

    /// 크루 해산. 크루 소유자만. 멤버·기기는 그대로 두어 늦게 온 멤버도 동기화로 해산을 받습니다.
    fn crew_disband(&mut self, user: &UserId) -> R {
        self.need_kind(SpaceKind::Crew)?;
        if *user != self.st.owner {
            return Err(AuthError::NotOwner);
        }
        self.st.disbanded = true;
        Ok(Effect::changed())
    }

    fn crew_request(&mut self, user: &UserId, name: &str) -> R {
        self.need_kind(SpaceKind::Space)?;
        if !name.is_empty() {
            check_name(name, MAX_NAME_CHARS, "크루 이름")?;
        }
        if self.st.pending_crew_conversion.is_some_and(|p| p != *user) {
            return Err(AuthError::ConversionPending);
        }
        self.st.pending_crew_conversion = Some(*user);
        self.crew_name = (!name.is_empty()).then(|| name.to_owned());
        Ok(Effect::changed())
    }

    fn crew_accept(&mut self, user: &UserId) -> R {
        self.need_kind(SpaceKind::Space)?;
        let requester = self
            .st
            .pending_crew_conversion
            .ok_or(AuthError::NoPendingConversion)?;
        if requester == *user {
            return Err(AuthError::Invalid("요청한 사람은 수락할 수 없음"));
        }
        if !self.st.is_member(&requester) {
            return Err(AuthError::NoPendingConversion);
        }
        // 미결정 사항의 기본값: 요청자가 소유자, 수락한 사람은 '관리자'.
        let admin = admin_role_id(&self.space);
        self.st.kind = SpaceKind::Crew;
        self.st.owner = requester;
        self.st.roles.insert(
            admin,
            Role {
                id: admin,
                name: ADMIN_ROLE_NAME.to_owned(),
                color: 0,
                rank: ADMIN_ROLE_RANK,
                permissions: Permissions::ADMINISTRATOR,
                automation: false,
            },
        );
        if let Some(m) = self.st.members.get_mut(user)
            && !m.roles.contains(&admin)
        {
            let m = Arc::make_mut(m);
            m.roles.push(admin);
            m.roles.sort_unstable();
        }
        if let Some(everyone) = self.st.roles.get_mut(&RoleId::EVERYONE) {
            everyone.permissions = Permissions::DEFAULT_EVERYONE;
        }
        // 기존 대화가 새 멤버에게 드러나지 않게 지금 있는 채널은 모두 과거 기록 비공개로 시작합니다(기획서 1:1→크루
        // 전환 3). 다시 공개하려면 두 사람이 모두 바꿔야 합니다(`legacy_history`).
        update_where(
            &mut self.st.channels,
            |c| c.history != HistoryVisibility::Private,
            |c| c.history = HistoryVisibility::Private,
        );
        self.legacy_channels = self.st.channels.keys().copied().collect();
        self.history_consent.clear();
        self.converted_by = Some((requester, *user));
        if let Some(name) = self.crew_name.take() {
            self.st.settings.name = name;
        }
        self.st.pending_crew_conversion = None;
        self.rank_change = RankChange::All;
        Ok(Effect::changed())
    }

    /// 소유권 이전. 앞 소유자 신원과 쓴 기기가 함께 서명한 인증서가 있어야 합니다: 체크포인트로 들어온 멤버는 이 이벤트를
    /// 열 수 없어, 체크포인트에 실린 인증서 사슬로 소유자가 바뀐 것을 확인합니다.
    fn ownership_transfer(
        &mut self,
        h: &EventHeader,
        new_owner: &UserId,
        cert: &OwnershipCert,
        cert_ok: impl FnOnce() -> bool,
    ) -> R {
        let user = &h.user;
        if *user != self.st.owner {
            return Err(AuthError::NotOwner);
        }
        if !self.st.is_member(new_owner) {
            return Err(AuthError::UnknownMember);
        }
        if new_owner == user {
            return Err(AuthError::Invalid("이미 소유자"));
        }
        // 인증서는 지금 바탕에 묶입니다: 소유권이 돌아온 뒤 옛 인증서를 다음 구간에 다시 싣지 못하게.
        if cert.space != self.space
            || cert.base != self.base.as_ref().map(|b| b.id)
            || cert.from != *user
            || cert.to != *new_owner
            || cert.device != h.author
        {
            return Err(AuthError::Invalid("소유권 인증서"));
        }
        if self.owner_certs.len() >= MAX_OWNER_CHAIN {
            return Err(AuthError::Limit("체크포인트 사이 소유권 이전 수"));
        }
        // 체크포인트 사슬만 보는 새 멤버가 첫 이전의 기기를 앞 체크포인트의 소유자 기기 목록으로 확인할 수 있게, 바탕
        // 뒤 첫 이전은 그 목록에서 활성이던 기기만 씁니다(그 뒤에 더한 기기는 다음 체크포인트를 기다림).
        if self.owner_certs.is_empty()
            && let Some(b) = &self.base
            && !b.owner_devices.contains_active(&h.author)
        {
            return Err(AuthError::Invalid("앞 체크포인트에 없던 소유자 기기"));
        }
        if !cert_ok() {
            return Err(AuthError::Invalid("소유권 인증서 서명"));
        }
        self.st.owner = *new_owner;
        self.owner_certs.push(cert.clone());
        // 위임은 그 소유자의 것이라 소유자가 바뀌면 사라집니다.
        self.delegation = None;
        self.rank_changed(user);
        self.rank_changed(new_owner);
        Ok(Effect::changed())
    }

    fn committer_change(&mut self, user: &UserId, device: &DeviceId) -> R {
        if *user != self.st.owner {
            return Err(AuthError::NotOwner);
        }
        let holder = self
            .device_owner
            .get(device)
            .ok_or(AuthError::InvalidCommitter)?;
        if *holder != self.st.owner
            && !self
                .st
                .base_permissions(holder)
                .contains(Permissions::ADMINISTRATOR)
        {
            return Err(AuthError::InvalidCommitter);
        }
        self.st.committer = *device;
        Ok(Effect::changed())
    }

    /// 멤버의 새 기기 목록. 대상은 목록의 사용자이고, 그 사용자의 기기뿐 아니라 다른 멤버도 전할 수 있습니다(신원 키
    /// 서명이 있으므로). 옛 빌드의 복구 기기는 아직 상태의 기기 목록에 없어 직접 쓸 수 없기 때문입니다.
    /// 새 목록은 지금 목록을 이어받아야 합니다([`DeviceList::extends`]: 기기를 빼거나 폐기를 되돌리지 못함).
    ///
    /// 판을 만든 기기(공동 서명)가 지금 목록에서 활성이어야 합니다(폐기된 기기가 신원 키로 기기를 더하지 못하게).
    /// 지금 목록에 없던 기기가 자기만 더한 복구 판은 다른 사용자가 전할 때만, 그리고 대상 말고 [`RECOVERY_RELAYS`]명의
    /// 멤버가 따로 전했을 때 받습니다(그 전의 전달은 기록만 함). 지금 목록에 OTP 키가 있으면 복구 판에 그 키의 서명이
    /// 있어야 합니다([`ListChange::OtpMissing`]이면 거절): 시드만 가진 사람은 멤버와 짜도 기기를 넣지 못합니다.
    fn device_list_update(&mut self, author: &UserId, f: &DevicesFacts) -> R {
        let list = &f.list;
        let target = list.user;
        if !self.st.is_member(&target) {
            return Err(AuthError::UnknownMember);
        }
        let current = self.st.devices.get(&target);
        if current.is_some_and(|l| list.version <= l.version) {
            return Err(AuthError::StaleDeviceList);
        }
        if current.is_some_and(|l| !list.record.extends(l)) {
            return Err(AuthError::BadDeviceList);
        }
        let recovery = match current
            .map(|cur| list.record.classify(list.cosigner, list.otp_signed, cur))
        {
            None | Some(ListChange::Authorized) => None,
            Some(ListChange::Recovery(dev)) if *author != target => Some(dev),
            Some(ListChange::Recovery(_) | ListChange::OtpMissing(_) | ListChange::Rejected) => {
                return Err(AuthError::BadDeviceList);
            }
        };
        self.check_member_devices(&target, list, None)?;
        if let Some(dev) = recovery {
            let key = (target, dev);
            if !self.recovery_relays.contains_key(&key)
                && self.recovery_relays.range(relays_of(target)).count()
                    >= MAX_RECOVERY_RELAYS_PER_USER
            {
                return Err(AuthError::Limit("복구 전달"));
            }
            let others = self.st.members.keys().filter(|u| **u != target).count();
            let needed = RECOVERY_RELAYS.min(others).max(1);
            let relayers = self.recovery_relays.entry(key).or_default();
            relayers.insert(*author);
            if relayers.len() < needed {
                return Ok(Effect::none());
            }
        }
        // 목록이 바뀌면(승인·거부·복구) 그 사용자의 모으던 복구 전달은 끝납니다(새 목록을 이어받지 않으므로).
        let done: Vec<_> = self
            .recovery_relays
            .range(relays_of(target))
            .map(|(k, _)| *k)
            .collect();
        for k in done {
            self.recovery_relays.remove(&k);
        }
        self.set_device_list(target, list.record.clone());
        Ok(Effect::changed())
    }
}

/// 덮어쓰기에서 바뀌는 권한 비트(추가·삭제·변경된 대상의 허용·거부 차이).
fn overwrite_delta(old: &[PermissionOverwrite], new: &[PermissionOverwrite]) -> Permissions {
    let mut bits = 0u64;
    for o in new {
        let (a, d) = old
            .iter()
            .find(|x| x.target == o.target)
            .map_or((0, 0), |p| (p.allow.0, p.deny.0));
        bits |= (a ^ o.allow.0) | (d ^ o.deny.0);
    }
    for o in old {
        if !new.iter().any(|x| x.target == o.target) {
            bits |= o.allow.0 | o.deny.0;
        }
    }
    Permissions(bits)
}

/// 보관 노드 접속 정보: 링크와 같은 모양(자기 IP 없음)과 크기 상한.
fn check_archive_contact(c: &ContactInfo) -> Result<(), AuthError> {
    const MAX_ADDRS: usize = 16;
    const MAX_ADDR_CHARS: usize = 512;
    if c.peer_id.is_empty()
        || c.peer_id.len() > 64
        || c.relay_addrs.len() > MAX_ADDRS
        || c.relay_addrs.iter().any(|a| a.len() > MAX_ADDR_CHARS)
        || c.onion.as_ref().is_some_and(|o| o.len() > MAX_ADDR_CHARS)
        || c.check_no_own_ip().is_err()
    {
        return Err(AuthError::Invalid("보관 노드 접속 정보"));
    }
    Ok(())
}

fn check_space_settings(s: &SpaceSettings) -> Result<(), AuthError> {
    check_name(&s.name, MAX_NAME_CHARS, "공간 이름")?;
    check_free_text(&s.description, MAX_DESCRIPTION_CHARS, "공간 설명")
}

fn check_role(role: &Role) -> Result<(), AuthError> {
    check_name(&role.name, MAX_NAME_CHARS, "역할 이름")?;
    if !(1..=MAX_ROLE_RANK).contains(&role.rank) {
        return Err(AuthError::Invalid("역할 순위"));
    }
    if role.color > 0x00FF_FFFF {
        return Err(AuthError::Invalid("역할 색"));
    }
    Ok(())
}

fn normalize_nickname(n: Option<&str>) -> Result<Option<String>, AuthError> {
    match n {
        None | Some("") => Ok(None),
        Some(s) => {
            check_name(s, MAX_NICKNAME_CHARS, "별명")?;
            Ok(Some(s.to_owned()))
        }
    }
}

/// 항목 수, 기기 중복, 이름 길이, 기기 ID가 실린 서명 공개키의 키 ID인지(서명은 [`SigMemo`]가 따로 검증). 키 ID
/// 확인은 해시 두 번이라 서명 확인보다 훨씬 싸므로, 엉뚱한 키를 실은 목록은 서명을 보기 전에 여기서 거절합니다.
pub(super) fn check_device_list_shape(sdl: &SignedDeviceList) -> Result<(), AuthError> {
    if sdl.list.devices.len() > MAX_DEVICE_ENTRIES {
        return Err(AuthError::BadDeviceList);
    }
    let unique: BTreeSet<&DeviceId> = sdl.list.devices.iter().map(|d| &d.device).collect();
    if unique.len() != sdl.list.devices.len() {
        return Err(AuthError::BadDeviceList);
    }
    if sdl
        .list
        .devices
        .iter()
        .any(|d| d.name.chars().count() > MAX_NAME_CHARS || !d.keys_match())
    {
        return Err(AuthError::BadDeviceList);
    }
    Ok(())
}

/// `recovery_relays`에서 한 사용자의 키 범위(사용자가 앞이라 이어져 있음).
fn relays_of(user: UserId) -> std::ops::RangeInclusive<(UserId, DeviceId)> {
    (user, DeviceId([0; 32]))..=(user, DeviceId([0xff; 32]))
}

/// 지속 맵에서 `pred`에 맞는 값만 `f`로 고칩니다. 맞지 않는 값의 마디는 해소기 스냅숏과 계속 나눠 씁니다(전부 돌며
/// 고쳐 쓰면 모든 마디를 복사함).
fn update_where<K: Ord + Clone, V: Clone>(
    map: &mut SharedMap<K, V>,
    pred: impl Fn(&V) -> bool,
    mut f: impl FnMut(&mut V),
) {
    let keys: Vec<K> = map
        .iter()
        .filter(|(_, v)| pred(v))
        .map(|(k, _)| k.clone())
        .collect();
    for k in keys {
        if let Some(v) = map.get_mut(&k) {
            f(v);
        }
    }
}

#[cfg(test)]
pub(super) mod synthetic {
    use super::*;
    use yangpa_core::device::DeviceRecord;
    use yangpa_core::model::BanInfo;
    use yangpa_core::time::Timestamp;

    fn id32(tag: u8, i: usize) -> [u8; 32] {
        let mut b = [tag; 32];
        b[..8].copy_from_slice(&(i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15).to_le_bytes());
        b
    }

    /// 서명 검사 없이 큰 Machine을 만듭니다(복제 비용 측정용). 사용자 `users`명, 두 명 중 한 명은 기기 2대.
    pub fn machine(users: usize) -> Machine {
        let mut members = SharedMap::new();
        let mut devices = SharedMap::new();
        let mut device_owner = SharedMap::new();
        let mut member_since = SharedMap::new();
        let mut used_proofs = imbl::OrdSet::new();
        for i in 0..users {
            let u = UserId(id32(1, i));
            members.insert(
                u,
                Arc::new(Member {
                    user: u,
                    roles: Vec::new(),
                    nickname: Some(format!("멤버{i}")),
                    automation: false,
                }),
            );
            let n = 1 + i % 2;
            // 키 약속·키 해시는 서명 검사를 거치지 않으므로 아무 값이나 둡니다(크기만 실제와 같게).
            let list = MemberDevices {
                version: 1,
                devices: (0..n)
                    .map(|k| DeviceRecord {
                        device: DeviceId(id32(2 + k as u8, i)),
                        added_at: Timestamp(1),
                        revoked_at: None,
                        ed_pk: id32(10 + k as u8, i),
                        pq_hash: id32(12 + k as u8, i),
                        key_commit: id32(14 + k as u8, i),
                    })
                    .collect(),
                otp: None,
            };
            for d in &list.devices {
                device_owner.insert(d.device, u);
            }
            devices.insert(u, Arc::new(list));
            member_since.insert(u, i as u64);
            used_proofs.insert((id32(5, i % 4), i as u64, u));
        }
        let owner = UserId(id32(1, 0));
        let mut bans = SharedMap::new();
        for i in 0..users / 30 {
            bans.insert(
                UserId(id32(6, i)),
                BanInfo {
                    reason: String::new(),
                    banned_by: owner,
                    banned_at: Timestamp(1),
                },
            );
        }
        let departed = (0..users / 10)
            .map(|i| {
                (
                    UserId(id32(7, i)),
                    Arc::new(MemberDevices {
                        version: 1,
                        devices: Vec::new(),
                        otp: None,
                    }),
                )
            })
            .collect();
        let mut roles = BTreeMap::new();
        roles.insert(
            RoleId::EVERYONE,
            Role::everyone(Permissions::DEFAULT_EVERYONE),
        );
        Machine {
            space: SpaceId([7; 16]),
            genesis: EventId([0; 32]),
            st: SpaceState {
                kind: SpaceKind::Crew,
                owner,
                committer: DeviceId(id32(2, 0)),
                settings: SpaceSettings {
                    name: "규모".into(),
                    description: String::new(),
                    icon: None,
                    icon_key: None,
                    allow_external_emoji: false,
                    strict_rotation: false,
                },
                members,
                roles,
                channels: SharedMap::new(),
                devices,
                invites: Default::default(),
                emojis: BTreeMap::new(),
                pending_crew_conversion: None,
                bans,
                archives: BTreeMap::new(),
                channel_transport: BTreeMap::new(),
                role_display: SharedMap::new(),
                anonymous_channels: Default::default(),
                crew_servers: Default::default(),
                forum_featured: Default::default(),
                attach_limits: Default::default(),
                volatile_caps: Default::default(),
                edit_rules: Default::default(),
                emoji_limits: Default::default(),
                emoji_by: Default::default(),
                disbanded: false,
            },
            deleted_channels: imbl::OrdSet::new(),
            deleted_roles: imbl::OrdSet::new(),
            device_owner,
            departed,
            member_since,
            crew_name: None,
            used_proofs,
            converted_by: None,
            legacy_channels: BTreeSet::new(),
            history_consent: BTreeMap::new(),
            recovery_relays: SharedMap::new(),
            rank_change: RankChange::None,
            limits: Limits::default(),
            keys: CrewKeys::new(DeviceId(id32(2, 0))),
            cur: EventId([0; 32]),
            checkpoint_seq: 0,
            seen_keys: imbl::OrdSet::new(),
            retired_keys: imbl::OrdMap::new(),
            revoked_since: imbl::OrdSet::new(),
            revocation_root: [0; 32],
            owner_certs: Vec::new(),
            delegation: None,
            disputes: imbl::OrdSet::new(),
            retired_invites: imbl::OrdSet::new(),
            base: None,
        }
    }

    /// 관리자가 한도까지 채운 설정: 역할 250개(표시 범위에 역할 전부), 채널 500개(덮어쓰기 100개, 주제 1024자, 포럼
    /// 태그 20개), 초대 1000개(역할 250개씩), 이모지 500개, 채널별 작은 표 500항목씩. 스냅숏이 따로 복사하는 작은 표의
    /// 크기를 재려고 씁니다.
    pub fn fill_admin_config(m: &mut Machine) {
        use yangpa_core::body::Secret32;
        use yangpa_core::ids::BlobId;
        use yangpa_core::model::{ForumTag, PermissionOverwrite};
        let role_ids: Vec<RoleId> = (1..MAX_ROLES as u16)
            .map(|i| {
                let mut b = [9u8; 16];
                b[..2].copy_from_slice(&i.to_le_bytes());
                RoleId(b)
            })
            .collect();
        let name = |n: usize| "가".repeat(n);
        for (i, id) in role_ids.iter().enumerate() {
            m.st.roles.insert(
                *id,
                Role {
                    id: *id,
                    name: name(MAX_NAME_CHARS),
                    color: 0,
                    rank: 1 + i as u32,
                    permissions: Permissions::KICK_MEMBERS,
                    automation: false,
                },
            );
            m.st.role_display.insert(
                *id,
                RoleDisplay {
                    visible_to: Some(role_ids.clone()),
                    see_admins: false,
                },
            );
        }
        let member = *m.st.members.keys().next().unwrap();
        for i in 0..MAX_CHANNELS {
            let mut b = [8u8; 16];
            b[..8].copy_from_slice(&(i as u64).to_le_bytes());
            let id = ChannelId(b);
            let mut s = ChannelSettings::text(name(MAX_NAME_CHARS));
            s.topic = name(MAX_TOPIC_CHARS);
            s.overwrites = (0..MAX_OVERWRITES)
                .map(|k| PermissionOverwrite {
                    target: if k == 0 {
                        OverwriteTarget::Member(member)
                    } else {
                        OverwriteTarget::Role(role_ids[k])
                    },
                    allow: Permissions::SEND_MESSAGES,
                    deny: Permissions::NONE,
                })
                .collect();
            s.forum_tags = (0..MAX_FORUM_TAGS as u32)
                .map(|k| ForumTag {
                    id: k,
                    name: name(MAX_NAME_CHARS),
                    emoji: None,
                })
                .collect();
            m.st.channels.insert(id, s);
            m.st.channel_transport.insert(id, ChannelTransport::P2p);
            m.st.anonymous_channels.insert(id);
            m.st.forum_featured.insert(id, EventId([1; 32]));
            m.st.attach_limits.insert(id, 1 << 20);
            m.st.edit_rules.insert(
                id,
                EditRules {
                    no_edit: true,
                    no_delete: false,
                },
            );
            m.legacy_channels.insert(id);
            m.history_consent.insert(id, member);
        }
        for i in 0..MAX_INVITES {
            m.st.invites.push_back(InviteState {
                info: InviteInfo {
                    token_hash: id32(10, i),
                    verify_key: [3; 32],
                    expires_at: None,
                    max_uses: None,
                    roles: role_ids.clone(),
                },
                created_by: member,
                uses: 0,
            });
        }
        for i in 0..MAX_EMOJIS {
            m.st.emojis.insert(
                EmojiId(i as u64),
                CustomEmoji {
                    id: EmojiId(i as u64),
                    name: "e".repeat(32),
                    animated: false,
                    blob: BlobId(id32(11, i)),
                    key: Secret32([4; 32]),
                    mime: "image/png".into(),
                },
            );
            m.st.emoji_by.insert(EmojiId(i as u64), member);
        }
    }

    /// 스냅숏마다 따로 복사되는 작은 표(지속 자료 구조가 아닌 것)의 직렬화 크기.
    pub fn copied_bytes(m: &Machine) -> usize {
        let st = &m.st;
        let parts = (
            (&st.roles, &st.emojis, &st.archives, &st.channel_transport),
            (&st.anonymous_channels, &st.crew_servers, &st.forum_featured),
            (&st.attach_limits, &st.edit_rules, &st.settings),
            (&st.emoji_limits, &st.emoji_by),
            (&m.legacy_channels, &m.history_consent, &m.crew_name),
        );
        postcard::to_stdvec(&parts).unwrap().len()
    }

    /// `cargo test --profile perf -p yangpa-sync machine_clone_cost -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn machine_clone_cost() {
        use std::time::Instant;
        const USERS: usize = 30_000;
        let m = machine(USERS);
        let mut times = Vec::new();
        for _ in 0..21 {
            let t = Instant::now();
            let c = m.clone();
            times.push(t.elapsed());
            drop(c);
        }
        times.sort();
        let c = m.st.clone();
        let t = Instant::now();
        assert!(c == m.st);
        let eq = t.elapsed();
        // 복제한 뒤의 첫 쓰기는 바뀐 경로의 마디를 복사합니다. 스냅숏 하나가 뒤에 남기는 비용을 보려고, 나눠 쓰는 중에
        // 멤버 1024명을 고칠 때와 이미 혼자 가진 뒤 같은 쓰기를 할 때를 견줍니다.
        let touch = |m: &mut Machine| {
            for i in 0..1024 {
                let u = UserId(id32(1, (i * 29) % USERS));
                if let Some(x) = m.st.members.get_mut(&u) {
                    let x = Arc::make_mut(x);
                    x.automation = !x.automation;
                }
                m.member_since.insert(u, i as u64);
            }
        };
        let mut a = m.clone();
        let t = Instant::now();
        touch(&mut a);
        let shared = t.elapsed();
        let t = Instant::now();
        touch(&mut a);
        let owned = t.elapsed();
        eprintln!(
            "멤버 {USERS}명 Machine 복제 중앙값 {:?}(최소 {:?}), 복제본 비교 {eq:?}, 복제 뒤 1024명 쓰기 {shared:?}(혼자 가진 뒤 {owned:?})",
            times[10], times[0]
        );
        // 관리자 설정을 한도까지 채우면: 채널·초대·역할 표시는 지속 자료 구조라 나눠 쓰고, 나머지 작은 표만 복사합니다.
        let mut full = m.clone();
        fill_admin_config(&mut full);
        let mut times = Vec::new();
        for _ in 0..21 {
            let t = Instant::now();
            let c = full.clone();
            times.push(t.elapsed());
            drop(c);
        }
        times.sort();
        let all = postcard::to_stdvec(&full.st).unwrap().len()
            - postcard::to_stdvec(&m.st).unwrap().len();
        eprintln!(
            "한도까지 채운 관리자 설정: 복제 중앙값 {:?}, 설정 직렬화 {}KB 가운데 스냅숏마다 복사되는 작은 표 {}KB",
            times[10],
            all / 1024,
            copied_bytes(&full) / 1024
        );
    }
}

#[cfg(test)]
mod shape_tests {
    use super::*;
    use yangpa_core::device::{BoxPublicKey, DeviceEntry, DeviceList, DeviceSignKeys};
    use yangpa_core::sign::{Sig, SigningKeypair};
    use yangpa_core::time::Timestamp;

    /// 기기 ID와 실린 서명 키가 맞지 않는 목록은 서명 없이(여기선 0으로 채운 서명) 구조 검사만으로 거절됩니다.
    #[test]
    fn shape_check_rejects_mismatched_device_id_without_signatures() {
        let dev = SigningKeypair::from_seed(&[1; 32]);
        let other = SigningKeypair::from_seed(&[2; 32]);
        let make = |sign_keys: DeviceSignKeys| SignedDeviceList {
            list: DeviceList {
                user: UserId([9; 32]),
                version: 1,
                devices: vec![DeviceEntry {
                    device: DeviceId(dev.public()),
                    box_key: BoxPublicKey::filled(1),
                    sign_keys,
                    name: "기기".into(),
                    added_at: Timestamp(1),
                    revoked_at: None,
                }],
                otp: None,
            },
            sig: Sig::from_bytes(&[0; Sig::LEN]).unwrap(),
            cosig: None,
            otp_sig: None,
        };
        assert!(check_device_list_shape(&make(DeviceSignKeys::of(&dev))).is_ok());
        assert_eq!(
            check_device_list_shape(&make(DeviceSignKeys::of(&other))),
            Err(AuthError::BadDeviceList)
        );
    }
}
