//! 이벤트 본문(암호화 전 평문). postcard로 직렬화한 뒤 패딩·암호화합니다.

use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::badge::SignedBadgeCard;
use crate::checkpoint::{DelegationCert, OwnershipCert, SignedCheckpoint};
use crate::device::{DeviceCosig, SignedDeviceList, SignedProfile};
use crate::error::CoreError;
use crate::event::EventKind;
use crate::ids::{
    BlobId, ChannelId, ChannelRootId, CheckpointId, DeviceId, EmojiId, EventId, KeyGroupId,
    LinkToken, RoleId, RootKeyId, SpaceId, StateKeyId, UserId,
};
use crate::keyupdate::KeyUpdatePayload;
use crate::model::{
    ChannelSettings, ChannelTransport, EmojiLimits, Flair, Member, Permissions, Role, RoleDisplay,
    SpaceKind, SpaceSettings,
};
use crate::sign::{Sig, SignDomain, SigningKeypair, verify};
use crate::time::Timestamp;

/// 32바이트 비밀값. 해제 시 0으로 덮어씁니다.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct Secret32(pub [u8; 32]);

impl std::fmt::Debug for Secret32 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret32(..)")
    }
}

impl Secret32 {
    pub fn random() -> Self {
        let mut b = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut b);
        Secret32(b)
    }
}

/// 첨부 파일. 원본은 블롭 스웜으로 받고, 이벤트에는 해시·키·미리보기만 싣습니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub blob: BlobId,
    /// 블롭 복호화 키(조각 단위 AEAD).
    pub key: Secret32,
    pub name: String,
    pub mime: String,
    pub size: u64,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// 보내는 쪽이 만든 작은 썸네일(WebP/PNG, 수 KB). 이벤트 본문과 함께 암호화됩니다.
    #[serde(default, with = "serde_bytes")]
    pub thumbnail: Vec<u8>,
}

/// 보내는 쪽이 만든 링크 미리보기. 받는 쪽은 외부 사이트에 접속하지 않습니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkEmbed {
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub site_name: String,
    #[serde(default, with = "serde_bytes")]
    pub thumbnail: Vec<u8>,
    /// 유튜브 같은 미디어 플레이어 주소. '눌러서 불러오기' 전에는 접속하지 않습니다.
    #[serde(default)]
    pub player_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MessageBody {
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub embeds: Vec<LinkEmbed>,
    #[serde(default)]
    pub reply_to: Option<EventId>,
    /// 포럼 게시글 안의 댓글이면 게시글 이벤트 ID.
    #[serde(default)]
    pub thread: Option<EventId>,
    #[serde(default)]
    pub mentions: Vec<UserId>,
}

/// 반응 이모지: 유니코드 또는 커스텀. JSON에서는 `{"type": "unicode", ...}`, 이벤트 본문(postcard)에서는
/// 바깥 태그 표현입니다.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(remote = "Self", tag = "type", rename_all = "snake_case")]
pub enum ReactionEmoji {
    Unicode {
        value: String,
    },
    Custom {
        id: EmojiId,
        name: String,
        animated: bool,
    },
}

/// [`ReactionEmoji`]의 이진 형식 표현.
#[allow(dead_code)]
#[derive(Serialize, Deserialize)]
#[serde(remote = "ReactionEmoji")]
enum ReactionEmojiBin {
    Unicode {
        value: String,
    },
    Custom {
        id: EmojiId,
        name: String,
        animated: bool,
    },
}

tagged_only_for_humans!(ReactionEmoji, ReactionEmojiBin);

/// 배포하는 키 하나. ID가 있는 키는 모두 ID가 키에서 나오고([`SharedKey::bound_to`]), 받는 쪽은 맞지 않는 것을 버립니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SharedKey {
    /// 상태 키 S(크루 그룹이면 공간 상태 키, 비공개 그룹이면 그 그룹의 상태 키).
    State { id: StateKeyId, key: Secret32 },
    /// 키 그룹의 기록 공개 뿌리 R(상태 키·공개 채널 뿌리가 여기서 나옴). 가입 전 기록을 넘겨줄 때나 갱신을 놓친 기기에게
    /// 멤버가 중계할 때 씁니다.
    Root {
        group: KeyGroupId,
        id: RootKeyId,
        key: Secret32,
    },
    /// 채널 뿌리 하나(그 채널의 기록 키가 여기서 나옴, `KeyRef::Record`). 기록 일부만 넘겨줄 때 씁니다.
    ChannelRoot {
        channel: ChannelId,
        id: ChannelRootId,
        key: Secret32,
    },
    /// 휘발성 채널 세션 키(RAM 전용 스트림으로만 전달).
    Volatile {
        channel: ChannelId,
        generation: u64,
        key: Secret32,
    },
    /// 키 그룹 갱신 `seq`의 기기 몫 봉인 묶음(트리 가입자의 잎·경로, `yangpa_crypto::keytree::DeviceBundle` 인코딩).
    /// 묶음 자체가 받는 기기에게 봉인돼 있고, 받는 쪽은 그 갱신의 `bundle_root` 증명이 맞을 때만 엽니다. 그래서 누가
    /// 실어 보냈든(확정 기기·중계한 멤버) 같습니다.
    Bundle {
        group: KeyGroupId,
        seq: u64,
        #[serde(with = "serde_bytes")]
        bundle: Vec<u8>,
    },
}

/// [`SharedKey::Bundle`] 하나의 크기 상한(봉투 약 1.9KB + 증명에 넉넉히).
pub const MAX_SHARED_BUNDLE_LEN: usize = 16 * 1024;

/// 상태 키의 ID: 공간과 키에서 나오는 해시 앞 16바이트. ID가 키에 묶여 있어, 상태 키를 나눌 수 있는 누구도 이미 쓰는
/// ID에 다른 키를 끼워 넣지 못합니다(키 저장은 먼저 온 것이 이기므로 끼워 넣으면 그 ID의 상태를 영영 못 읽음).
pub fn state_key_id(space: &SpaceId, key: &Secret32) -> StateKeyId {
    let mut h = blake3::Hasher::new_derive_key("yangpa/v1/state-key-id");
    h.update(space.as_bytes());
    h.update(&key.0);
    let mut id = [0u8; 16];
    id.copy_from_slice(&h.finalize().as_bytes()[..16]);
    StateKeyId(id)
}

/// 채널 뿌리의 ID = derive_key("yangpa/v3/channel-root-id", 공간 ‖ 채널 ‖ 뿌리)의 앞 16바이트. 상태 키 ID와 같은
/// 까닭으로 키에 묶습니다(`yangpa_crypto::keysched::channel_root_id`도 이것을 부름).
pub fn channel_root_id(space: &SpaceId, channel: &ChannelId, root: &Secret32) -> ChannelRootId {
    let mut h = blake3::Hasher::new_derive_key("yangpa/v3/channel-root-id");
    h.update(space.as_bytes());
    h.update(channel.as_bytes());
    h.update(&root.0);
    let mut id = [0u8; 16];
    id.copy_from_slice(&h.finalize().as_bytes()[..16]);
    ChannelRootId(id)
}

/// 키 그룹 뿌리 R의 ID = derive_key("yangpa/v3/group-root-id", 공간 ‖ 그룹 ‖ R)의 앞 16바이트.
pub fn group_root_id(space: &SpaceId, group: &KeyGroupId, root: &Secret32) -> RootKeyId {
    let mut h = blake3::Hasher::new_derive_key("yangpa/v3/group-root-id");
    h.update(space.as_bytes());
    h.update(group.as_bytes());
    h.update(&root.0);
    let mut id = [0u8; 16];
    id.copy_from_slice(&h.finalize().as_bytes()[..16]);
    RootKeyId(id)
}

impl SharedKey {
    /// 이 공간에서 받아도 되는 키인지: ID가 있는 키는 ID가 (공간, 그룹·채널, 키)에서 나온 것이어야 합니다. 키 저장은 먼저
    /// 온 것이 이기므로, 키를 나눌 수 있는 누구도 이미 쓰는 ID에 다른 키를 끼워 넣지 못하게 합니다.
    pub fn bound_to(&self, space: &SpaceId) -> bool {
        match self {
            SharedKey::State { id, key } => *id == state_key_id(space, key),
            SharedKey::Root { group, id, key } => *id == group_root_id(space, group, key),
            SharedKey::ChannelRoot { channel, id, key } => {
                *id == channel_root_id(space, channel, key)
            }
            SharedKey::Volatile { .. } | SharedKey::Bundle { .. } => true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyShareBody {
    pub keys: Vec<SharedKey>,
    /// 특정 사용자에게 주는 키 묶음이면 그 사용자(봉인 본문일 때).
    #[serde(default)]
    pub for_user: Option<UserId>,
}

/// 초대(상태에 기록되는 권위 있는 정보). 링크에는 토큰만 들어가고 여기엔 해시(식별용)와 토큰에서 만든 서명 키의
/// 공개 키(가입 증명 확인용)만 둡니다. 토큰 원문은 상태·이벤트 어디에도 싣지 않습니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteInfo {
    pub token_hash: [u8; 32],
    /// [`crate::links::invite_verify_key`]: 가입자가 낸 [`InviteProof`]를 이 키로 확인합니다.
    pub verify_key: [u8; 32],
    pub expires_at: Option<Timestamp>,
    pub max_uses: Option<u32>,
    #[serde(default)]
    pub roles: Vec<RoleId>,
}

/// 초대로 들어온다는 증명. 토큰 원문 대신 토큰에서 만든 서명 키로 (공간, 초대, 만든 시각, 가입자 기기 목록)에
/// 서명합니다. 상태 레인에는 이 증명만 남으므로, 다른 멤버가 보더라도 다른 사람(다른 기기 목록)을 그 초대로
/// 들이지 못하고, 같은 증명을 다시 쓰는 것은 해소기가 막습니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteProof {
    /// 사용한 초대(`InviteInfo::token_hash`).
    pub token_hash: [u8; 32],
    /// 증명을 만든 시각(같은 사람이 나갔다 다시 들어올 때 증명이 달라지게).
    pub at: Timestamp,
    pub sig: Sig,
    /// 가입하는 기기가 같은 내용에 기기 키로 한 서명. 토큰을 아는 사람(초대를 만든 사람)도 만들 수 없으므로, 그 사용자가
    /// 스스로 들어왔다는 증거가 됩니다. 옛 빌드의 복구 기기가 다른 멤버가 알려 준 크루를 다시 받아들일 때
    /// 봅니다(남이 나를 넣어 지어낸 크루와 가리려고).
    #[serde(default)]
    pub joiner: Option<DeviceCosig>,
}

impl InviteProof {
    fn message(
        space: &SpaceId,
        token_hash: &[u8; 32],
        at: Timestamp,
        devices: &SignedDeviceList,
    ) -> Result<Vec<u8>, CoreError> {
        let mut m = Vec::with_capacity(16 + 32 + 8 + 256);
        m.extend_from_slice(space.as_bytes());
        m.extend_from_slice(token_hash);
        m.extend_from_slice(&at.0.to_le_bytes());
        // 사용자 ID가 든 기기 목록 전체를 묶어, 같은 증명으로 다른 사람·다른 기기를 들이지 못하게 합니다.
        m.extend_from_slice(&postcard::to_stdvec(&devices.list)?);
        Ok(m)
    }

    /// 토큰을 가진 가입자가 만듭니다.
    pub fn create(
        token: &LinkToken,
        space: &SpaceId,
        devices: &SignedDeviceList,
        at: Timestamp,
    ) -> Result<Self, CoreError> {
        let token_hash = crate::links::token_hash(token);
        let msg = Self::message(space, &token_hash, at, devices)?;
        let sig = crate::links::invite_keypair(token).sign(SignDomain::InviteProof, &msg);
        Ok(InviteProof {
            token_hash,
            at,
            sig,
            joiner: None,
        })
    }

    /// 가입하는 기기가 기기 키로 함께 서명합니다([`Self::joiner`]).
    pub fn signed_by_joiner(
        mut self,
        space: &SpaceId,
        devices: &SignedDeviceList,
        device: &SigningKeypair,
    ) -> Result<Self, CoreError> {
        let msg = Self::message(space, &self.token_hash, self.at, devices)?;
        self.joiner = Some(DeviceCosig {
            device: DeviceId(device.public()),
            sig: device.sign(SignDomain::InviteJoiner, &msg),
        });
        Ok(self)
    }

    /// 가입 기기 서명이 맞으면 그 기기(`devices`에 있는 기기여야 함).
    pub fn joined_device(&self, space: &SpaceId, devices: &SignedDeviceList) -> Option<DeviceId> {
        let j = self.joiner.as_ref()?;
        if !devices.list.devices.iter().any(|d| d.device == j.device) {
            return None;
        }
        let msg = Self::message(space, &self.token_hash, self.at, devices).ok()?;
        verify(j.device.as_bytes(), SignDomain::InviteJoiner, &msg, &j.sig).ok()?;
        Some(j.device)
    }

    /// 초대의 공개 키(`InviteInfo::verify_key`)로 확인합니다.
    pub fn verify(
        &self,
        verify_key: &[u8; 32],
        space: &SpaceId,
        devices: &SignedDeviceList,
    ) -> Result<(), CoreError> {
        let msg = Self::message(space, &self.token_hash, self.at, devices)?;
        verify(verify_key, SignDomain::InviteProof, &msg, &self.sig)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomEmoji {
    pub id: EmojiId,
    pub name: String,
    pub animated: bool,
    pub blob: BlobId,
    pub key: Secret32,
    pub mime: String,
}

/// 본문. 종류는 헤더의 [`EventKind`]와 일치해야 합니다.
// 직렬화 형식이 계약이라 변형을 Box로 감싸지 않습니다(크기 차이는 수신 경로에서 문제 되지 않음).
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventBody {
    Message(MessageBody),
    Edit {
        target: EventId,
        text: String,
        #[serde(default)]
        embeds: Vec<LinkEmbed>,
    },
    Delete {
        target: EventId,
    },
    Reaction {
        target: EventId,
        emoji: ReactionEmoji,
    },
    ReactionRemove {
        target: EventId,
        emoji: ReactionEmoji,
    },
    ForumPost {
        title: String,
        tags: Vec<u32>,
        first: MessageBody,
    },
    ForumPostUpdate {
        post: EventId,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        tags: Option<Vec<u32>>,
        #[serde(default)]
        pinned: Option<bool>,
        #[serde(default)]
        locked: Option<bool>,
    },
    KeyShare(KeyShareBody),
    SpaceCreate {
        kind: SpaceKind,
        settings: SpaceSettings,
        owner: UserId,
        committer: DeviceId,
        everyone: Permissions,
        /// 처음 멤버(1대1이면 두 사람).
        members: Vec<Member>,
        /// 처음 멤버들의 서명된 기기 목록.
        devices: Vec<SignedDeviceList>,
    },
    SpaceUpdate {
        settings: SpaceSettings,
    },
    ChannelCreate {
        channel: ChannelId,
        settings: ChannelSettings,
    },
    ChannelUpdate {
        channel: ChannelId,
        settings: ChannelSettings,
    },
    ChannelDelete {
        channel: ChannelId,
    },
    RoleCreate {
        role: Role,
    },
    RoleUpdate {
        role: Role,
    },
    RoleDelete {
        role: RoleId,
    },
    MemberJoin {
        member: Member,
        devices: SignedDeviceList,
        #[serde(default)]
        profile: Option<SignedProfile>,
        /// 초대로 들어왔다면 가입자가 만든 초대 증명(토큰 원문은 싣지 않음).
        #[serde(default)]
        invite: Option<InviteProof>,
    },
    MemberUpdate {
        user: UserId,
        roles: Vec<RoleId>,
        #[serde(default)]
        nickname: Option<String>,
    },
    MemberKick {
        user: UserId,
        #[serde(default)]
        reason: String,
    },
    MemberLeave {
        user: UserId,
    },
    InviteCreate {
        invite: InviteInfo,
    },
    InviteRevoke {
        token_hash: [u8; 32],
    },
    EmojiAdd {
        emoji: CustomEmoji,
    },
    EmojiRemove {
        id: EmojiId,
    },
    CrewConvertRequest {
        name: String,
    },
    CrewConvertAccept,
    /// 소유권 이전. 인증서는 앞 소유자 신원과 이 이벤트를 쓴 기기가 서명한 것으로, 체크포인트 사슬(`owner_chain`)에 실려
    /// 새 멤버가 소유자가 바뀐 것을 확인합니다.
    OwnershipTransfer {
        new_owner: UserId,
        cert: OwnershipCert,
    },
    CommitterChange {
        device: DeviceId,
    },
    DeviceListUpdate {
        devices: SignedDeviceList,
    },
    /// 작성자 자신의 멤버 정보: 크루 별명(`None` = 그대로, 빈 문자열 = 지움)과 서명된 전역 프로필(멤버에게 퍼뜨림).
    /// 역할은 담지 않으므로 동시에 바뀐 역할을 되돌리지 않습니다(`MemberUpdate`와 달리 남을 바꿀 수 없음).
    MemberProfile {
        #[serde(default)]
        nickname: Option<String>,
        #[serde(default)]
        profile: Option<SignedProfile>,
    },
    /// 크루에서 내보내고(멤버였다면) 밴 목록에 올립니다. 밴된 사용자의 `MemberJoin`은 거부됩니다.
    MemberBan {
        user: UserId,
        #[serde(default)]
        reason: String,
        /// 있으면 이 시각 이후 그 사용자가 쓴 메시지를 받은 노드마다 지웁니다(최근 메시지 삭제).
        #[serde(default)]
        delete_since: Option<Timestamp>,
    },
    /// 밴 목록에서 뺍니다. 다시 들어오려면 새 초대가 필요합니다.
    MemberUnban {
        user: UserId,
    },
    /// 보관 전용 노드 지정(같은 기기면 내용을 바꿈). `MANAGE_SPACE`, 크루에서만.
    ArchiveGrant(crate::archive::ArchiveGrant),
    /// 보관 전용 노드 지정 해제.
    ArchiveRevoke {
        device: DeviceId,
    },
    /// 채널 내용 전송 방식 지정. `MANAGE_CHANNELS`, 텍스트·통화 채널만, 지금 값과 달라야 함.
    // postcard는 변형 번호로 풀므로 새 변형은 늘 맨 끝에 붙입니다.
    ChannelTransportSet {
        channel: ChannelId,
        transport: ChannelTransport,
    },
    /// 역할 표시 설정(누가 이 역할을 보는지, 이 역할이 관리자를 보는지). `MANAGE_ROLES`와 그 역할보다 높은 순위.
    RoleDisplaySet {
        role: RoleId,
        display: RoleDisplay,
    },
    /// 채널 익명 설정. `MANAGE_CHANNELS`, 카테고리가 아닌 채널만, 지금 값과 달라야 함. 켜진 동안 쓴 글은 헤더 종류가
    /// `AnonMessage`·`AnonForumPost`라 꺼도 익명으로 남습니다.
    ChannelAnonymousSet {
        channel: ChannelId,
        anonymous: bool,
    },
    /// 크루 STUN 서버 목록 바꾸기(목록 전체). `turn`은 TURN을 없앤(2026-11-07) 뒤 늘 빈 목록으로 씁니다(postcard 형식이라
    /// 칸을 빼면 옛 이벤트를 못 읽음). `MANAGE_SPACE`, 크루만. 목록마다 [`crate::model::MAX_CREW_SERVERS`]줄,
    /// 줄마다 [`crate::model::MAX_CREW_SERVER_LEN`]바이트까지(꼴은 쓰는 쪽 데몬이 검사하고, 쓰는 데몬도 다시 거름).
    CrewServersSet {
        stun: Vec<String>,
        turn: Vec<String>,
    },
    /// 포럼 대문 게시글 정하기(`None`이면 대문 없음). 그 채널의 `MANAGE_CHANNELS`, 포럼 채널만, 지금 값과 달라야 함.
    /// 게시글은 메시지 레인이라 상태 해소기는 게시글이 있는지 모르고 ID만 둡니다(보기가 없는 글을 거름).
    ForumFeaturedSet {
        channel: ChannelId,
        post: Option<EventId>,
    },
    /// 채널 첨부 한도(첨부 하나의 평문 바이트, `None`이면 무제한). 크루만, 그 채널의 `MANAGE_CHANNELS`, 카테고리가
    /// 아닌 채널만, [`crate::model::MIN_ATTACH_LIMIT`] 이상, 지금 값과 달라야 함. 보내는 쪽 데몬이 지키고 받는 쪽은
    /// 넘는 첨부를 저절로 받지 않습니다.
    ChannelAttachLimitSet {
        channel: ChannelId,
        max_bytes: Option<u64>,
    },
    /// 채널의 수정·삭제 금지. 크루만, 그 채널의 `MANAGE_CHANNELS`, 카테고리가 아닌 채널만, 지금 값과 달라야 함.
    /// 수정 금지면 아무도 본문을 고치지 못하고(게시글 제목·태그는 `MANAGE_MESSAGES`만), 삭제 금지면 작성자가 자기
    /// 메시지를 지우지 못합니다(`MANAGE_MESSAGES`의 운영 삭제는 됨). 받는 쪽도 메시지 권한 검사에서 지킵니다.
    ChannelEditRulesSet {
        channel: ChannelId,
        no_edit: bool,
        no_delete: bool,
    },
    /// 표 예비 보유 기기 지정(`None`이면 해제). 소유자만, 소유자·관리자 멤버의 활성 기기이고 확정 기기가 아니어야 하며
    /// 지금 값과 달라야 함. 예비 기기는 확정 기기와 함께 크루 표(leaf_master·모든 그룹 키)를 받아, 확정 기기를 잃어도
    /// 전체 재키 없이 이어 갈 수 있게 합니다. 지정한 기기는 다음 크루 전체 재키까지 표 보유 기기로 셉니다.
    KeyBackupSet {
        device: Option<DeviceId>,
    },
    /// 키 그룹 갱신 한 조각(`docs/protocol.md` "키 그룹"). 조각을 잇는 법과 검사는 `yangpa-sync`의 해소기가 정합니다.
    KeyUpdate(KeyUpdatePayload),
    /// 상태 체크포인트(코드 74). 소유자의 활성 기기(또는 위임받은 기기)가 쓰고 소유자 신원이 함께 서명합니다.
    StateCheckpoint(Box<SignedCheckpoint>),
    /// 체크포인트 작성 위임(코드 76). 소유자의 활성 기기만, 위임 인증서는 소유자 신원과 그 기기가 서명.
    CheckpointDelegation(Box<DelegationCert>),
    /// 체크포인트 이견(코드 77, 정보용): 내가 계산한 상태 해시가 다름. 기기마다 바탕 체크포인트당 한 번.
    CheckpointDispute {
        checkpoint: CheckpointId,
        seq: u64,
        state_hash: [u8; 32],
    },
    /// 휘발성 채널 세션 키 회전 공지(코드 21, RAM 전용). 키는 싣지 않고 약속값만 싣습니다(`docs/protocol.md` "휘발성 채널").
    VolatileRotation(VolatileRotationBody),
    /// 상시 휘발성 채널의 참가 기기 상한(코드 78, `None`이면 기본값 [`crate::model::DEFAULT_VOLATILE_CAP`]). 크루만, 그
    /// 채널의 `MANAGE_CHANNELS`, 휘발성 채널만, [`crate::model::MIN_VOLATILE_CAP`]..=[`crate::model::MAX_VOLATILE_CAP`],
    /// 지금 값과 달라야 함. 상한을 넘는 기기는 그 채널에 들어가지 못합니다(방식을 저절로 바꾸지 않음).
    ChannelVolatileCapSet {
        channel: ChannelId,
        cap: Option<u32>,
    },
    /// 크루 커스텀 이모지 한도(코드 79). 크루만, `MANAGE_SPACE`(이모지 관리 권한으로는 못 바꿈), 개수는
    /// 1..=`MAX_EMOJIS`, 크기는 1바이트 이상, 지금 값과 달라야 함. 개수는 해소기가, 크기는 등록하는 쪽과 받는 쪽이 지킵니다
    /// (`EmojiAdd`에는 크기가 없음).
    EmojiLimitsSet {
        limits: EmojiLimits,
    },
    /// 작성자 자신의 후원자 표시(코드 100, [`Flair`]). 멤버면 받기만 하고 공간 상태에는 넣지 않습니다(상태 해시가 빌드마다
    /// 달라지지 않게). 데몬이 사용자마다 따로 모읍니다.
    MemberFlair(Flair),
    /// 작성자 자신의 서명된 뱃지 카드(코드 101). 상태를 바꾸지 않고 멤버에게 퍼뜨리기만 합니다(`MemberProfile`의 프로필과
    /// 같음). 멤버여야 하고 카드의 사용자가 작성자여야 합니다.
    MemberBadges {
        card: SignedBadgeCard,
    },
    /// 크루 해산(코드 102). 크루 소유자만. 적용되면 그 뒤 상태 이벤트는 모두 거부되고 아무도 권한이 없으며, 멤버 기기는
    /// 기록을 남겨 둔 채(늦게 온 멤버에게 해산을 전함) 각자 나가기로 지웁니다.
    CrewDisband,
}

/// 휘발성 세션 키 회전 공지. 회전한 기기가 서명하고 지금 참가자에게 직접 보냅니다. 키는 참가자마다 따로 미는 감싸기
/// (`YpRequest::VolatileKeyWrap`)로 가거나(새 키, 가입자), 부모 세대 키에서 각자 유도합니다(가입만 있을 때).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolatileRotationBody {
    /// 새 세대(무작위, 0 아님).
    pub generation: u64,
    /// 부모 세대(받는 기기 목록·가입 유도의 기준). 없으면 처음부터.
    pub parent: Option<u64>,
    /// 새 무작위 키인지. 거짓이면 가입만 있어 부모 세대 키에서 유도합니다(`keysched::vol_join`).
    pub fresh: bool,
    /// 부모 세대 받는 기기에 더한 기기(가입자), 오름차순.
    pub added: Vec<DeviceId>,
    /// 부모 세대 받는 기기에서 뺀 기기(새 키일 때만), 오름차순.
    pub removed: Vec<DeviceId>,
    /// 새 세대 받는 기기 목록 요약(`keysched::vol_recipients_hash`).
    pub recipients_hash: [u8; 32],
    /// 키 약속값 C(`keysched::vol_key_commit`). 받는 쪽은 이와 맞는 키만 씁니다.
    pub commit: [u8; 32],
}

/// 회전 공지 하나에 싣는 더한·뺀 기기 수 상한. 부모 없는 첫 세대는 참가자 전부를 더한 목록에 실으므로 상시 참여 상한
/// (`MAX_VOLATILE_CAP` 2000)보다 커야 하고, 입장·퇴장 채널도 이만큼의 기기까지만 한 세대로 묶을 수 있습니다.
pub const MAX_VOLATILE_ROTATION_DELTA: usize = 4096;
const _: () = assert!(MAX_VOLATILE_ROTATION_DELTA > crate::model::MAX_VOLATILE_CAP as usize);

impl VolatileRotationBody {
    /// 구조 검사: 세대 0 없음, 목록은 오름차순·겹침 없음·상한 안, 가입 유도면 부모가 있고 뺀 기기 없음.
    pub fn validate(&self) -> Result<(), CoreError> {
        let sorted = |v: &[DeviceId]| v.windows(2).all(|w| w[0] < w[1]);
        let ok = self.generation != 0
            && self.parent != Some(self.generation)
            && self.added.len() <= MAX_VOLATILE_ROTATION_DELTA
            && self.removed.len() <= MAX_VOLATILE_ROTATION_DELTA
            && sorted(&self.added)
            && sorted(&self.removed)
            && !self
                .added
                .iter()
                .any(|d| self.removed.binary_search(d).is_ok())
            && (self.fresh
                || (self.parent.is_some() && self.removed.is_empty() && !self.added.is_empty()));
        if ok {
            Ok(())
        } else {
            Err(CoreError::InvalidEncoding)
        }
    }
}

impl EventBody {
    pub fn kind(&self) -> EventKind {
        match self {
            EventBody::Message(_) => EventKind::Message,
            EventBody::Edit { .. } => EventKind::Edit,
            EventBody::Delete { .. } => EventKind::Delete,
            EventBody::Reaction { .. } => EventKind::Reaction,
            EventBody::ReactionRemove { .. } => EventKind::ReactionRemove,
            EventBody::ForumPost { .. } => EventKind::ForumPost,
            EventBody::ForumPostUpdate { .. } => EventKind::ForumPostUpdate,
            EventBody::KeyShare(_) => EventKind::KeyShare,
            EventBody::SpaceCreate { .. } => EventKind::SpaceCreate,
            EventBody::SpaceUpdate { .. } => EventKind::SpaceUpdate,
            EventBody::ChannelCreate { .. } => EventKind::ChannelCreate,
            EventBody::ChannelUpdate { .. } => EventKind::ChannelUpdate,
            EventBody::ChannelDelete { .. } => EventKind::ChannelDelete,
            EventBody::RoleCreate { .. } => EventKind::RoleCreate,
            EventBody::RoleUpdate { .. } => EventKind::RoleUpdate,
            EventBody::RoleDelete { .. } => EventKind::RoleDelete,
            EventBody::MemberJoin { .. } => EventKind::MemberJoin,
            EventBody::MemberUpdate { .. } => EventKind::MemberUpdate,
            EventBody::MemberProfile { .. } => EventKind::MemberProfile,
            EventBody::MemberKick { .. } => EventKind::MemberKick,
            EventBody::MemberLeave { .. } => EventKind::MemberLeave,
            EventBody::MemberBan { .. } => EventKind::MemberBan,
            EventBody::MemberUnban { .. } => EventKind::MemberUnban,
            EventBody::ArchiveGrant(_) => EventKind::ArchiveGrant,
            EventBody::ArchiveRevoke { .. } => EventKind::ArchiveRevoke,
            EventBody::ChannelTransportSet { .. } => EventKind::ChannelTransportSet,
            EventBody::RoleDisplaySet { .. } => EventKind::RoleDisplaySet,
            EventBody::ChannelAnonymousSet { .. } => EventKind::ChannelAnonymousSet,
            EventBody::CrewServersSet { .. } => EventKind::CrewServersSet,
            EventBody::ForumFeaturedSet { .. } => EventKind::ForumFeaturedSet,
            EventBody::ChannelAttachLimitSet { .. } => EventKind::ChannelAttachLimitSet,
            EventBody::ChannelEditRulesSet { .. } => EventKind::ChannelEditRulesSet,
            EventBody::InviteCreate { .. } => EventKind::InviteCreate,
            EventBody::InviteRevoke { .. } => EventKind::InviteRevoke,
            EventBody::EmojiAdd { .. } => EventKind::EmojiAdd,
            EventBody::EmojiRemove { .. } => EventKind::EmojiRemove,
            EventBody::CrewConvertRequest { .. } => EventKind::CrewConvertRequest,
            EventBody::CrewConvertAccept => EventKind::CrewConvertAccept,
            EventBody::OwnershipTransfer { .. } => EventKind::OwnershipTransfer,
            EventBody::CommitterChange { .. } => EventKind::CommitterChange,
            EventBody::DeviceListUpdate { .. } => EventKind::DeviceListUpdate,
            EventBody::KeyBackupSet { .. } => EventKind::KeyBackupSet,
            EventBody::KeyUpdate(_) => EventKind::KeyUpdate,
            EventBody::StateCheckpoint(_) => EventKind::StateCheckpoint,
            EventBody::CheckpointDelegation(_) => EventKind::CheckpointDelegation,
            EventBody::CheckpointDispute { .. } => EventKind::CheckpointDispute,
            EventBody::VolatileRotation(_) => EventKind::VolatileRotation,
            EventBody::ChannelVolatileCapSet { .. } => EventKind::ChannelVolatileCapSet,
            EventBody::EmojiLimitsSet { .. } => EventKind::EmojiLimitsSet,
            EventBody::MemberFlair(_) => EventKind::MemberFlair,
            EventBody::MemberBadges { .. } => EventKind::MemberBadges,
            EventBody::CrewDisband => EventKind::CrewDisband,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, CoreError> {
        Ok(postcard::to_stdvec(self)?)
    }

    /// 이 본문을 헤더 종류 `kind`로 실을 수 있는지. 익명 글(`AnonMessage`…)은 원래 종류의 본문을 씁니다.
    pub fn fits(&self, kind: EventKind) -> bool {
        self.kind() == kind.base()
    }

    /// 복호화한 평문을 해석하고 헤더 종류와 맞는지 확인합니다. 키 갱신 조각은 구조 검사까지 합니다(목록 개수는
    /// 디코딩 중에 이미 묶임).
    pub fn decode(bytes: &[u8], expected: EventKind) -> Result<Self, CoreError> {
        let body: EventBody = postcard::from_bytes(bytes)?;
        if !body.fits(expected) {
            return Err(CoreError::InvalidEncoding);
        }
        if let EventBody::KeyUpdate(p) = &body {
            p.validate()?;
        }
        if let EventBody::VolatileRotation(r) = &body {
            r.validate()?;
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device_list(user_key: &crate::sign::SigningKeypair) -> SignedDeviceList {
        let dev = crate::sign::SigningKeypair::generate();
        crate::device::DeviceList {
            user: UserId(user_key.public()),
            version: 1,
            devices: vec![crate::device::DeviceEntry {
                device: DeviceId(dev.public()),
                box_key: crate::device::BoxPublicKey::filled(7),
                sign_keys: crate::device::DeviceSignKeys::of(&dev),
                name: "기기".into(),
                added_at: Timestamp(1),
                revoked_at: None,
            }],
            otp: None,
        }
        .sign(user_key)
        .unwrap()
    }

    #[test]
    fn invite_proof_binds_space_and_devices_without_revealing_token() {
        let token = LinkToken::random();
        let key = crate::links::invite_verify_key(&token);
        let space = SpaceId::random();
        let carol = device_list(&crate::sign::SigningKeypair::generate());
        let dave = device_list(&crate::sign::SigningKeypair::generate());
        let proof = InviteProof::create(&token, &space, &carol, Timestamp(10)).unwrap();
        proof.verify(&key, &space, &carol).unwrap();
        assert_eq!(proof.token_hash, crate::links::token_hash(&token));
        // 다른 사람·다른 공간·다른 초대 키로는 통하지 않습니다.
        assert!(proof.verify(&key, &space, &dave).is_err());
        assert!(proof.verify(&key, &SpaceId::random(), &carol).is_err());
        let other = crate::links::invite_verify_key(&LinkToken::random());
        assert!(proof.verify(&other, &space, &carol).is_err());
        // 증명·공개 키 어디에도 토큰 원문이 없습니다.
        let bytes = postcard::to_stdvec(&proof).unwrap();
        assert!(!bytes.windows(16).any(|w| w == token.as_bytes()));
        assert!(!key.windows(16).any(|w| w == token.as_bytes()));
        // 만든 시각이 다르면 증명도 다릅니다(다시 들어올 때 새 증명).
        let again = InviteProof::create(&token, &space, &carol, Timestamp(11)).unwrap();
        assert_ne!(again.sig, proof.sig);
    }

    /// 가입 기기 서명은 목록에 있는 그 기기의 키로만 만들 수 있고, 공간·목록에 묶입니다(토큰만 아는 사람은 못 만듦).
    #[test]
    fn invite_joiner_signature_needs_the_joining_device() {
        let token = LinkToken::random();
        let space = SpaceId::random();
        let user = crate::sign::SigningKeypair::generate();
        let dev = crate::sign::SigningKeypair::generate();
        let list = crate::device::DeviceList {
            user: UserId(user.public()),
            version: 1,
            devices: vec![crate::device::DeviceEntry {
                device: DeviceId(dev.public()),
                box_key: crate::device::BoxPublicKey::filled(7),
                sign_keys: crate::device::DeviceSignKeys::of(&dev),
                name: "기기".into(),
                added_at: Timestamp(1),
                revoked_at: None,
            }],
            otp: None,
        }
        .sign(&user)
        .unwrap();
        let bare = InviteProof::create(&token, &space, &list, Timestamp(10)).unwrap();
        assert_eq!(bare.joined_device(&space, &list), None);
        let signed = bare.clone().signed_by_joiner(&space, &list, &dev).unwrap();
        assert_eq!(
            signed.joined_device(&space, &list),
            Some(DeviceId(dev.public()))
        );
        // 토큰 서명은 그대로라 가입 확인에는 영향이 없습니다.
        signed
            .verify(&crate::links::invite_verify_key(&token), &space, &list)
            .unwrap();
        // 다른 공간이나 목록에 없는 기기의 서명은 증거가 아닙니다.
        assert_eq!(signed.joined_device(&SpaceId::random(), &list), None);
        let stranger = bare
            .signed_by_joiner(&space, &list, &crate::sign::SigningKeypair::generate())
            .unwrap();
        assert_eq!(stranger.joined_device(&space, &list), None);
    }

    #[test]
    fn body_roundtrip_and_kind_check() {
        let b = EventBody::Message(MessageBody {
            text: "안녕 <:yangpa:123>".into(),
            ..Default::default()
        });
        let bytes = b.encode().unwrap();
        assert_eq!(EventBody::decode(&bytes, EventKind::Message).unwrap(), b);
        assert!(EventBody::decode(&bytes, EventKind::Edit).is_err());
    }

    /// 태그 붙은 enum(채팅 수명, 권한 덮어쓰기 대상, 반응 이모지)이 든 본문도 postcard로 풀리고,
    /// JSON 표현은 태그 그대로입니다.
    #[test]
    fn tagged_enums_roundtrip_in_postcard_and_keep_json_shape() {
        use crate::model::{ChannelSettings, ChatTtl, OverwriteTarget, PermissionOverwrite};

        let mut settings = ChannelSettings::text("일반");
        settings.ttl = ChatTtl::Seconds(30);
        settings.overwrites = vec![
            PermissionOverwrite {
                target: OverwriteTarget::Role(RoleId::EVERYONE),
                allow: Permissions::NONE,
                deny: Permissions::SEND_MESSAGES,
            },
            PermissionOverwrite {
                target: OverwriteTarget::Member(UserId::random()),
                allow: Permissions::SEND_MESSAGES,
                deny: Permissions::NONE,
            },
        ];
        let bodies = [
            EventBody::ChannelCreate {
                channel: ChannelId::random(),
                settings: settings.clone(),
            },
            EventBody::Reaction {
                target: EventId::random(),
                emoji: ReactionEmoji::Unicode {
                    value: "👍".into()
                },
            },
            EventBody::ReactionRemove {
                target: EventId::random(),
                emoji: ReactionEmoji::Custom {
                    id: EmojiId(42),
                    name: "yangpa".into(),
                    animated: true,
                },
            },
        ];
        for b in bodies {
            let bytes = b.encode().unwrap();
            assert_eq!(EventBody::decode(&bytes, b.kind()).unwrap(), b);
        }
        for ttl in [ChatTtl::None, ChatTtl::OnLeave, ChatTtl::Seconds(7)] {
            let bytes = postcard::to_stdvec(&ttl).unwrap();
            assert_eq!(postcard::from_bytes::<ChatTtl>(&bytes).unwrap(), ttl);
        }

        let v = serde_json::to_value(ChatTtl::Seconds(30)).unwrap();
        assert_eq!(v, serde_json::json!({"mode": "seconds", "seconds": 30}));
        assert_eq!(
            serde_json::from_value::<ChatTtl>(v).unwrap(),
            ChatTtl::Seconds(30)
        );
        let v = serde_json::to_value(ReactionEmoji::Unicode {
            value: "👍".into()
        })
        .unwrap();
        assert_eq!(v, serde_json::json!({"type": "unicode", "value": "👍"}));
        let v = serde_json::to_value(settings.overwrites[0]).unwrap();
        assert_eq!(v["target"]["type"], "role");
        let back: ChannelSettings =
            serde_json::from_value(serde_json::to_value(&settings).unwrap()).unwrap();
        assert_eq!(back, settings);
    }

    /// 새 변형은 맨 끝에 붙이므로 이미 퍼진 본문의 바이트(변형 번호 + 필드)는 그대로 풀립니다. 형식 3판에서 옛 MLS
    /// 변형 둘(커밋·웰컴)을 빼며 번호를 한 번 당겼습니다(3판 헤더만 받으므로 옛 본문이 다른 뜻으로 풀리지 않음).
    #[test]
    fn old_bodies_keep_their_bytes_and_transport_set_comes_last() {
        let mut delete = vec![12, 16];
        delete.extend([1; 16]);
        let mut revoke = vec![33, 32];
        revoke.extend([2; 32]);
        let golden = [
            (
                delete,
                EventBody::ChannelDelete {
                    channel: ChannelId([1; 16]),
                },
            ),
            (vec![25], EventBody::CrewConvertAccept),
            (
                revoke,
                EventBody::ArchiveRevoke {
                    device: DeviceId([2; 32]),
                },
            ),
        ];
        for (bytes, body) in golden {
            assert_eq!(EventBody::decode(&bytes, body.kind()).unwrap(), body);
            assert_eq!(body.encode().unwrap(), bytes);
        }

        let set = EventBody::ChannelTransportSet {
            channel: ChannelId([3; 16]),
            transport: ChannelTransport::P2p,
        };
        let bytes = set.encode().unwrap();
        assert_eq!(bytes[0], 34, "ChannelTransportSet은 34번째 변형");
        assert_eq!(bytes.last(), Some(&1), "P2p = 1");
        assert_eq!(
            EventBody::decode(&bytes, EventKind::ChannelTransportSet).unwrap(),
            set
        );
        assert!(EventBody::decode(&bytes, EventKind::ChannelUpdate).is_err());

        let display = EventBody::RoleDisplaySet {
            role: RoleId([4; 16]),
            display: RoleDisplay {
                visible_to: Some(vec![RoleId([5; 16])]),
                see_admins: false,
            },
        };
        let bytes = display.encode().unwrap();
        assert_eq!(bytes[0], 35, "RoleDisplaySet은 35번째 변형");
        assert_eq!(
            EventBody::decode(&bytes, EventKind::RoleDisplaySet).unwrap(),
            display
        );

        let anon = EventBody::ChannelAnonymousSet {
            channel: ChannelId([6; 16]),
            anonymous: true,
        };
        let bytes = anon.encode().unwrap();
        assert_eq!(bytes[0], 36, "ChannelAnonymousSet은 36번째 변형");
        assert_eq!(bytes.last(), Some(&1));
        assert_eq!(
            EventBody::decode(&bytes, EventKind::ChannelAnonymousSet).unwrap(),
            anon
        );

        let servers = EventBody::CrewServersSet {
            stun: vec!["stun.example.org:3478".into()],
            turn: vec!["turn:u:p@turn.example.org:3478".into()],
        };
        let bytes = servers.encode().unwrap();
        assert_eq!(bytes[0], 37, "CrewServersSet은 37번째 변형");
        assert_eq!(
            EventBody::decode(&bytes, EventKind::CrewServersSet).unwrap(),
            servers
        );

        let featured = EventBody::ForumFeaturedSet {
            channel: ChannelId([6; 16]),
            post: Some(EventId([2; 32])),
        };
        let bytes = featured.encode().unwrap();
        assert_eq!(bytes[0], 38, "ForumFeaturedSet은 38번째 변형");
        assert_eq!(
            EventBody::decode(&bytes, EventKind::ForumFeaturedSet).unwrap(),
            featured
        );

        let limit = EventBody::ChannelAttachLimitSet {
            channel: ChannelId([7; 16]),
            max_bytes: Some(8 * 1024 * 1024),
        };
        let bytes = limit.encode().unwrap();
        assert_eq!(bytes[0], 39, "ChannelAttachLimitSet은 39번째 변형");
        assert_eq!(
            EventBody::decode(&bytes, EventKind::ChannelAttachLimitSet).unwrap(),
            limit
        );

        let rules = EventBody::ChannelEditRulesSet {
            channel: ChannelId([8; 16]),
            no_edit: true,
            no_delete: false,
        };
        let bytes = rules.encode().unwrap();
        assert_eq!(bytes[0], 40, "ChannelEditRulesSet은 40번째 변형");
        assert_eq!(
            EventBody::decode(&bytes, EventKind::ChannelEditRulesSet).unwrap(),
            rules
        );

        let backup = EventBody::KeyBackupSet {
            device: Some(DeviceId([9; 32])),
        };
        let bytes = backup.encode().unwrap();
        assert_eq!(bytes[0], 41, "KeyBackupSet은 41번째 변형");
        assert_eq!(
            EventBody::decode(&bytes, EventKind::KeyBackupSet).unwrap(),
            backup
        );
        assert!(EventBody::decode(&bytes, EventKind::CommitterChange).is_err());
    }

    fn key_update_sample() -> KeyUpdatePayload {
        use crate::keyupdate::{KeyUpdateKind, RCt, SealedBox};
        KeyUpdatePayload {
            group: KeyGroupId([1; 16]),
            seq: 2,
            prev: Some(EventId([2; 32])),
            kind: KeyUpdateKind::Rotate {
                removed: vec![DeviceId([3; 32])],
                joined: Vec::new(),
            },
            root_commit: [4; 32],
            prev_root_commit: [5; 32],
            crew: None,
            bundle_root: None,
            shards: None,
            depth: 0,
            salt: [6; 24],
            layout: Vec::new(),
            wraps: Vec::new(),
            sealed: vec![SealedBox(vec![7; 10])],
            r_ct: Some(RCt { ct: [8; 48] }),
            part: 0,
            parts: 1,
        }
    }

    /// 키 갱신 본문은 42번 변형(뒤로 체크포인트 셋)이고, 풀 때 조각 구조 검사까지 합니다.
    #[test]
    fn key_update_body_is_validated() {
        let body = EventBody::KeyUpdate(key_update_sample());
        let bytes = body.encode().unwrap();
        assert_eq!(bytes[0], 42, "KeyUpdate는 42번째 변형");
        assert_eq!(
            EventBody::decode(&bytes, EventKind::KeyUpdate).unwrap(),
            body
        );
        assert!(EventBody::decode(&bytes, EventKind::KeyBackupSet).is_err());
        // 구조가 틀린 조각(새 R인데 r_ct 없음)은 풀리지 않습니다.
        let mut bad = key_update_sample();
        bad.r_ct = None;
        let bytes = EventBody::KeyUpdate(bad).encode().unwrap();
        assert!(EventBody::decode(&bytes, EventKind::KeyUpdate).is_err());
    }

    #[test]
    fn anonymous_kinds_carry_the_base_body() {
        let msg = EventBody::Message(MessageBody {
            text: "익명".into(),
            ..Default::default()
        });
        let bytes = msg.encode().unwrap();
        assert!(msg.fits(EventKind::AnonMessage) && msg.fits(EventKind::Message));
        assert_eq!(
            EventBody::decode(&bytes, EventKind::AnonMessage).unwrap(),
            msg
        );
        // 다른 종류의 익명 표시에는 맞지 않습니다.
        assert!(EventBody::decode(&bytes, EventKind::AnonForumPost).is_err());
        let post = EventBody::ForumPost {
            title: "제목".into(),
            tags: Vec::new(),
            first: MessageBody::default(),
        };
        assert!(post.fits(EventKind::AnonForumPost) && !post.fits(EventKind::AnonMessage));
        let delete = EventBody::Delete {
            target: EventId([1; 32]),
        };
        assert!(!delete.fits(EventKind::AnonMessage));
    }

    /// 그룹 뿌리·채널 뿌리 ID도 키에 묶입니다: 같은 ID에 다른 키, 다른 공간·그룹·채널의 키는 받지 않습니다.
    #[test]
    fn root_ids_are_bound_to_their_key() {
        let space = SpaceId([1; 16]);
        let group = KeyGroupId([2; 16]);
        let channel = ChannelId([3; 16]);
        let key = Secret32([7; 32]);
        let root = SharedKey::Root {
            group,
            id: group_root_id(&space, &group, &key),
            key: key.clone(),
        };
        assert!(root.bound_to(&space));
        assert!(!root.bound_to(&SpaceId([9; 16])));
        let SharedKey::Root { id, .. } = root else {
            unreachable!()
        };
        assert!(
            !SharedKey::Root {
                group: KeyGroupId([5; 16]),
                id,
                key: key.clone()
            }
            .bound_to(&space)
        );
        assert!(
            !SharedKey::Root {
                group,
                id,
                key: Secret32([8; 32])
            }
            .bound_to(&space)
        );
        let ch = SharedKey::ChannelRoot {
            channel,
            id: channel_root_id(&space, &channel, &key),
            key: key.clone(),
        };
        assert!(ch.bound_to(&space));
        let SharedKey::ChannelRoot { id, .. } = ch else {
            unreachable!()
        };
        assert!(
            !SharedKey::ChannelRoot {
                channel: ChannelId([4; 16]),
                id,
                key: key.clone()
            }
            .bound_to(&space)
        );
        assert!(
            !SharedKey::ChannelRoot {
                channel,
                id,
                key: Secret32([8; 32])
            }
            .bound_to(&space)
        );
        // 두 ID는 라벨이 달라 같은 입력에서도 다릅니다.
        assert_ne!(
            channel_root_id(&space, &ChannelId(group.0), &key).0,
            group_root_id(&space, &group, &key).0
        );
    }

    #[test]
    fn state_key_ids_are_bound_to_their_key() {
        let space = SpaceId([1; 16]);
        let key = Secret32([7; 32]);
        let id = state_key_id(&space, &key);
        assert_eq!(id, state_key_id(&space, &key));
        assert!(
            SharedKey::State {
                id,
                key: key.clone()
            }
            .bound_to(&space)
        );
        // 같은 ID에 다른 키, 다른 공간의 키는 받지 않습니다.
        assert!(
            !SharedKey::State {
                id,
                key: Secret32([8; 32])
            }
            .bound_to(&space)
        );
        assert!(!SharedKey::State { id, key }.bound_to(&SpaceId([2; 16])));
    }

    /// 형식 3판의 본문 변형 번호(postcard 위치). 두 갈래(체크포인트·키 그룹)를 합칠 때 정한 배치라 바뀌면 안 됩니다
    /// (`docs/decisions.md` "체크포인트와 키 그룹 합치기"). 인증서 없는 예전 소유권 이전은 2판 헤더째로 거부되므로
    /// 26번 자리는 인증서가 있는 판이 씁니다.
    #[test]
    fn v3_body_variant_indices_are_fixed() {
        fn index(b: &EventBody) -> u8 {
            postcard::to_stdvec(b).unwrap()[0]
        }
        let user = UserId([9; 32]);
        let cert = OwnershipCert {
            space: SpaceId([1; 16]),
            base: None,
            from: user,
            to: user,
            at: Timestamp(0),
            device: DeviceId([2; 32]),
            sig: Sig::from_bytes(&[0; Sig::LEN]).unwrap(),
            device_sig: Sig::from_bytes(&[0; Sig::LEN]).unwrap(),
        };
        let transfer = EventBody::OwnershipTransfer {
            new_owner: user,
            cert,
        };
        assert_eq!(index(&transfer), 26);
        assert_eq!(
            index(&EventBody::CommitterChange {
                device: DeviceId([2; 32])
            }),
            27
        );
        assert_eq!(
            index(&EventBody::DeviceListUpdate {
                devices: device_list(&SigningKeypair::generate())
            }),
            28
        );
        assert_eq!(index(&EventBody::KeyBackupSet { device: None }), 41);
        assert_eq!(
            index(&EventBody::CheckpointDispute {
                checkpoint: CheckpointId([0; 32]),
                seq: 1,
                state_hash: [0; 32],
            }),
            45
        );
        let rotation = EventBody::VolatileRotation(VolatileRotationBody {
            generation: 7,
            parent: None,
            fresh: true,
            added: vec![DeviceId([2; 32])],
            removed: Vec::new(),
            recipients_hash: [0; 32],
            commit: [0; 32],
        });
        assert_eq!(index(&rotation), 46);
        // 상한 가득(2000기기)의 첫 세대도 모양 검사를 통과합니다.
        let mut all: Vec<DeviceId> = (0..2000u32)
            .map(|i| {
                let mut b = [0u8; 32];
                b[..4].copy_from_slice(&i.to_be_bytes());
                DeviceId(b)
            })
            .collect();
        all.sort();
        let full = EventBody::VolatileRotation(VolatileRotationBody {
            generation: 9,
            parent: None,
            fresh: true,
            added: all,
            removed: Vec::new(),
            recipients_hash: [0; 32],
            commit: [0; 32],
        });
        let bytes = full.encode().unwrap();
        assert_eq!(
            EventBody::decode(&bytes, EventKind::VolatileRotation).unwrap(),
            full
        );
        assert_eq!(
            index(&EventBody::ChannelVolatileCapSet {
                channel: ChannelId([3; 16]),
                cap: Some(50),
            }),
            47
        );
        let limits = EventBody::EmojiLimitsSet {
            limits: EmojiLimits {
                max_count: Some(50),
                max_bytes: Some(1 << 20),
            },
        };
        assert_eq!(index(&limits), 48);
        assert_eq!(
            EventBody::decode(&limits.encode().unwrap(), EventKind::EmojiLimitsSet).unwrap(),
            limits
        );
        let badges = EventBody::MemberBadges {
            card: crate::badge::BadgeCard::default()
                .sign(&crate::sign::SigningKeypair::generate())
                .unwrap(),
        };
        assert_eq!(index(&badges), 50);
        assert_eq!(
            EventBody::decode(&badges.encode().unwrap(), EventKind::MemberBadges).unwrap(),
            badges
        );
        assert_eq!(index(&EventBody::CrewDisband), 51);
        assert_eq!(
            EventBody::decode(
                &EventBody::CrewDisband.encode().unwrap(),
                EventKind::CrewDisband
            )
            .unwrap(),
            EventBody::CrewDisband
        );
        // 2판의 MLS 종류(80·81)는 다시 쓰지 않으므로 이 판에서는 모르는 종류입니다.
        assert_eq!(EventKind::from_code(80), EventKind::Other(80));
        assert_eq!(EventKind::from_code(81), EventKind::Other(81));
        let flair = EventBody::MemberFlair(Flair {
            tier: Some(crate::model::FlairTier {
                banner: true,
                halo: false,
                until: Timestamp(9),
                at: Timestamp(1),
            }),
            halo: Some(crate::model::FlairHalo {
                colors: Some([0xE8541E, 0xFFFFFF]),
                at: Timestamp(2),
            }),
        });
        assert_eq!(index(&flair), 49);
        assert_eq!(
            EventBody::decode(&flair.encode().unwrap(), EventKind::MemberFlair).unwrap(),
            flair
        );
    }
}
