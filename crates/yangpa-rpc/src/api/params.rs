//! 메서드별 인자·결과 구조체. 이름은 `<무엇>Params`, `<무엇>Result`.
//!
//! 여러 메서드가 같은 모양을 쓰면 공용 구조체(`SpaceParams`, `SpaceChannelParams` …)를 씁니다.
//! 비밀값(토큰·기기 연결 링크)은 [`SecretString`]이라 `Debug`에 나오지 않습니다.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use yangpa_core::device::{DeviceEntry, Profile};
use yangpa_core::ids::{
    BlobId, CallId, ChannelId, DeviceId, EmojiId, EventId, RoleId, SpaceId, UserId,
};
use yangpa_core::model::{
    ChannelKind, EmojiLimits, ForumTag, HistoryVisibility, JoinMode, PermissionOverwrite,
    SecurityLevel,
};
use yangpa_core::time::Timestamp;
use yangpa_core::wire::SignalPayload;

use super::Scope;
use super::serde_util::{SecretString, TokenHash, double_option, emoji_id_str};
use super::types::{
    BridgeKind, DataType, GapView, IceServer, MessageView, NetStatus, PresenceMode, QrCode,
    ReportAction, ReportStatus, ShownBadgeView, SupporterProvider, Topology, TorStatus,
    VoiceParticipant,
};

/// `blob.read` 한 번에 읽을 수 있는 최대 바이트 수(base64·바이너리 공통).
pub const BLOB_READ_MAX_LEN: u64 = 8 * 1024 * 1024;
/// `message.list`의 최대 `limit`.
pub const MESSAGE_LIST_MAX: u32 = 200;
/// `message.list`의 기본 `limit`.
pub const MESSAGE_LIST_DEFAULT: u32 = 50;

// ───────────── 공용 ─────────────

/// `{space}`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceParams {
    pub space: SpaceId,
}

/// `{space?}`
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OptSpaceParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space: Option<SpaceId>,
}

/// `{space, channel}`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceChannelParams {
    pub space: SpaceId,
    pub channel: ChannelId,
}

/// `{space, user}`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceUserParams {
    pub space: SpaceId,
    pub user: UserId,
}

/// `crew.transfer_ownership`: `{space, user}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferOwnershipParams {
    pub space: SpaceId,
    pub user: UserId,
}

/// `supporter.unlink`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupporterUnlinkParams {
    pub provider: SupporterProvider,
}

/// `flair.set`. 후광 그라데이션의 두 색(0xRRGGBB), `null`이면 끔. 월 50달러 이상 후원자만.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlairSetParams {
    #[serde(default)]
    pub halo: Option<[u32; 2]>,
}

/// `badge.set_shown`: 이름 앞뒤에 보일 뱃지(모은 것 중 2개까지, 빈 목록이면 보이지 않음).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BadgeSetShownParams {
    pub shown: Vec<ShownBadgeView>,
}

/// `presence.set`. 준 것만 바꿉니다. `duration_secs`는 `mode`와 함께(온라인이 아닌 모드만, 없으면 바꿀 때까지).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceSetParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<PresenceMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u64>,
    /// 입력 없음(앱이 기기의 마지막 입력 시각을 보고 알림).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hide_last_seen: Option<bool>,
}

/// `device.link.create`: 새 기기에 넣을 링크와 그 QR.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceLinkOffer {
    pub url: SecretString,
    pub qr: QrCode,
    pub expires_at: Timestamp,
}

/// 승인을 기다리는 새 기기. `code`는 새 기기 화면의 번호와 같아야 합니다. `contested`면 다른 기기도 같은 링크를 썼습니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceLinkRequestView {
    pub name: String,
    pub code: String,
    pub contested: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceLinkDecideParams {
    pub approve: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceLinkJoinParams {
    pub url: SecretString,
}

/// `device.link.join`: 이 기기 화면에 띄울 번호.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceLinkJoined {
    pub code: String,
}

/// `{user}`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserParams {
    pub user: UserId,
}

/// `{user, device}`: 남의 계정의 기기 하나(`user.block_device`·`user.unblock_device`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserDeviceParams {
    pub user: UserId,
    pub device: DeviceId,
}

/// `{url}`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UrlParams {
    pub url: String,
}

/// `{id}` (토큰·사람 확인 ID)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdParams {
    pub id: String,
}

/// `{space}` 결과.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceRef {
    pub space: SpaceId,
}

/// `{channel}` 결과.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelRef {
    pub channel: ChannelId,
}

/// `{id}` 결과(이벤트 ID).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRef {
    pub id: EventId,
}

/// `{url}` 결과.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UrlResult {
    pub url: String,
}

// ───────────── 시스템·인증 ─────────────

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloParams {
    pub token: SecretString,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloResult {
    pub scopes: Vec<Scope>,
    pub automation: bool,
    /// 허용 공간(비면 전체). docs/api.md 보충.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spaces: Vec<SpaceId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionResult {
    pub version: String,
    /// 로컬 API 프로토콜 번호.
    pub protocol: u32,
}

/// `update.check`: 공개 저장소의 최신 정식 릴리스.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateInfo {
    /// 이 데몬의 판.
    pub current: String,
    /// 최신 릴리스 판(`x.y.z`).
    pub latest: String,
    /// `latest`가 `current`보다 새 판인지.
    pub newer: bool,
}

/// `update.download`: 그 판의 릴리스 파일(`YANGPA_<판>_…`) 하나를 데이터 폴더 `updates/`로 받습니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateDownloadParams {
    pub version: String,
    pub asset: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<UserId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<DeviceId>,
    pub net: NetStatus,
    pub spaces: u32,
    pub tor: TorStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeakcheckParams {
    pub marker: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeakcheckResult {
    pub clean: bool,
    #[serde(default)]
    pub findings: Vec<String>,
}

// ───────────── 신원·기기 ─────────────

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityCreateParams {
    pub name: String,
}

/// `{user, device}`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityRef {
    pub user: UserId,
    pub device: DeviceId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityView {
    pub user: UserId,
    pub device: DeviceId,
    pub profile: Profile,
    #[serde(default)]
    pub devices: Vec<DeviceEntry>,
    /// 내 기기로 연결된 뒤 첫 동기화 중(끝나면 `device.synced`).
    #[serde(default)]
    pub first_syncing: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRevokeParams {
    pub device: DeviceId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRenameParams {
    pub device: DeviceId,
    pub name: String,
}

/// `crew.set_committer`: 확정 기기로 지정할 기기(소유자·관리자의 활성 기기).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitterSetParams {
    pub space: SpaceId,
    pub device: DeviceId,
}

// ───────────── 프로필·메모 ─────────────

#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProfileGetParams {
    /// 없으면 내 프로필.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<UserId>,
}

/// 빈 문자열 경로(`avatar_path: ""`)는 이미지를 지웁니다.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProfileSetParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bio: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub banner_path: Option<String>,
    /// 0xRRGGBB. `null`이면 테마 색을 지웁니다.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub theme_color: Option<Option<u32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileSetCrewParams {
    pub space: SpaceId,
    /// 없으면 크루 닉네임을 지웁니다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteView {
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteSetParams {
    pub user: UserId,
    pub text: String,
}

// ───────────── 친구 ─────────────

#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FriendLinkCreateParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub single_use: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendLinkCreateResult {
    pub url: String,
    pub token_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenIdParams {
    pub token_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FriendAddStatus {
    /// 상대 수락 대기.
    Pending,
    Connected,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendAddResult {
    pub status: FriendAddStatus,
    pub user: UserId,
}

/// `friend.propose` 결과.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FriendProposeStatus {
    /// 상대에게 친구 요청을 보냄(수락 대기).
    Requested,
    /// 상대도 이미 친구 추가를 보내 둬서 바로 친구가 됨.
    Friends,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendProposeResult {
    pub status: FriendProposeStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestIdParams {
    pub request_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafetyNumberView {
    pub digits: String,
    pub verified: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendVerifyParams {
    pub user: UserId,
    pub verified: bool,
}

// ───────────── 공간·크루 ─────────────

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrewCreateParams {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrewUpdateParams {
    pub space: SpaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 빈 문자열이면 아이콘을 지웁니다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_path: Option<String>,
    /// 엄격한 키 회전(`SpaceSettings::strict_rotation`): 켜면 나간 기기를 뺀 키 갱신이 올 때까지 메시지를 보내지 않고
    /// 기다립니다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict_rotation: Option<bool>,
    /// 커스텀 이모지 한도(전체를 바꿈, `manage_space`). 안의 값이 `null`이면 그 한도 없음(개수는 500까지).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emoji_limits: Option<EmojiLimits>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrewConvertRequestParams {
    pub space: SpaceId,
    /// 크루 이름.
    pub name: String,
}

/// `member.search`가 고르는 멤버.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberFilter {
    #[default]
    All,
    /// 지금 인증된 연결이 있는 멤버(나 포함).
    Online,
    /// 온라인이거나 최근 7일 안에 이 기기와 연결된 적이 있는 멤버.
    Recent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberSearchParams {
    pub space: SpaceId,
    /// 표시 이름·별명·사용자 ID 앞부분(대소문자 무시). 비우면 모두.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default)]
    pub filter: MemberFilter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
    /// 기본 100, 최대 200.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberKickParams {
    pub space: SpaceId,
    pub user: UserId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberSetRolesParams {
    pub space: SpaceId,
    pub user: UserId,
    pub roles: Vec<RoleId>,
}

// ───────────── 채널 ─────────────

/// 채널 만들기. 휘발성 채널의 `link_previews` 기본값은 꺼짐입니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelCreateParams {
    pub space: SpaceId,
    pub name: String,
    pub kind: ChannelKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ChannelId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security: Option<SecurityLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join_mode: Option<JoinMode>,
    /// 채팅 수명(초). `ttl_on_leave`와 함께 쓰지 않습니다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u32>,
    /// 퇴장 시 삭제.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_on_leave: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<HistoryVisibility>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_previews: Option<bool>,
    /// 지정하면 이 역할들만 볼 수 있는 비공개 채널(@everyone의 보기 권한 거부).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_roles: Option<Vec<RoleId>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<i32>,
    /// 포럼 채널의 태그 목록(포럼이 아닌 채널이면 비어 있어야 함).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forum_tags: Option<Vec<ForumTag>>,
    /// `true`면 P2P 전용 채널(텍스트·음성만, 셀프 공간 불가).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p2p: Option<bool>,
    /// 역할·멤버별 권한 덮어쓰기 전체. `private_roles`와 함께 쓸 수 없습니다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overwrites: Option<Vec<PermissionOverwrite>>,
    /// `true`면 익명 채널(크루에서만, 카테고리는 안 됨). 글쓴이가 '익명'으로 보입니다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anonymous: Option<bool>,
    /// 첨부 하나의 최대 바이트(`null`이면 무제한, 0이면 첨부 금지). 크루만, 0 또는 1MiB 이상. 빠지면 무제한이고, 크루의
    /// 휘발성 채널만 10MiB입니다.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub attach_limit: Option<Option<u64>>,
    /// `true`면 아무도 메시지를 고칠 수 없습니다(크루만, 카테고리는 안 됨).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_edit: Option<bool>,
    /// `true`면 작성자가 자기 메시지를 지울 수 없습니다(운영 삭제는 됨. 크루만, 카테고리는 안 됨).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_delete: Option<bool>,
}

/// 채널 바꾸기. 빠진 필드는 그대로 둡니다. 종류(`kind`)는 바꿀 수 없습니다.
///
/// - `parent: null` → 최상위로.
/// - `ttl_secs: 0` → 수명 없음. `ttl_on_leave: true` → 퇴장 시 삭제.
/// - `private_roles: []` → 공개 채널로.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelUpdateParams {
    pub space: SpaceId,
    pub channel: ChannelId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub parent: Option<Option<ChannelId>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security: Option<SecurityLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join_mode: Option<JoinMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_on_leave: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<HistoryVisibility>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_previews: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_roles: Option<Vec<RoleId>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<i32>,
    /// 포럼 태그 목록을 통째로 바꿉니다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forum_tags: Option<Vec<ForumTag>>,
    /// 권한 덮어쓰기 목록을 통째로 바꿉니다(빈 목록이면 모두 지움). `private_roles`와 함께 쓸 수 없습니다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overwrites: Option<Vec<PermissionOverwrite>>,
    /// 익명 켜기·끄기. 꺼도 켜져 있는 동안 쓴 글은 익명으로 남고, 통화 채널이면 그 순간 모든 참가자가 통화에서 나옵니다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anonymous: Option<bool>,
    /// 첨부 하나의 최대 바이트(`null`이면 무제한, 0이면 첨부 금지, 빠지면 그대로). 크루만, 0 또는 1MiB 이상.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub attach_limit: Option<Option<u64>>,
    /// 수정 금지 켜기·끄기(크루만).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_edit: Option<bool>,
    /// 삭제 금지 켜기·끄기(크루만).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_delete: Option<bool>,
    /// 상시 휘발성 채널의 참가 기기 상한(`null`이면 기본 200, 빠지면 그대로). 크루만, 2..=2000.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub volatile_cap: Option<Option<u32>>,
}

impl ChannelUpdateParams {
    /// 바꿀 필드가 하나도 없는 인자.
    pub fn new(space: SpaceId, channel: ChannelId) -> Self {
        ChannelUpdateParams {
            space,
            channel,
            name: None,
            parent: None,
            topic: None,
            security: None,
            join_mode: None,
            ttl_secs: None,
            ttl_on_leave: None,
            history: None,
            link_previews: None,
            private_roles: None,
            position: None,
            forum_tags: None,
            overwrites: None,
            anonymous: None,
            attach_limit: None,
            no_edit: None,
            no_delete: None,
            volatile_cap: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowApiReadParams {
    pub space: SpaceId,
    pub channel: ChannelId,
    pub allowed: bool,
}

/// `channel.set_p2p`: `enabled`가 참이면 P2P 전용, 거짓이면 Tor로.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelSetP2pParams {
    pub space: SpaceId,
    pub channel: ChannelId,
    pub enabled: bool,
}

/// `trust.set_user`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustSetUserParams {
    pub user: UserId,
    pub enabled: bool,
}

/// `trust.set_space`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustSetSpaceParams {
    pub space: SpaceId,
    pub enabled: bool,
}

/// `channel.mark_read`. `event`가 없으면 채널의 가장 최근 메시지까지(글타래 답글·포럼 글 포함) 읽음으로.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkReadParams {
    pub space: SpaceId,
    pub channel: ChannelId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<EventId>,
}

// ───────────── 메시지 ─────────────

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageListParams {
    pub space: SpaceId,
    pub channel: ChannelId,
    /// 이 메시지보다 앞(오래된 쪽)만.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<EventId>,
    /// 최대 [`MESSAGE_LIST_MAX`]. 없으면 [`MESSAGE_LIST_DEFAULT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// 포럼 게시글 안 댓글.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<EventId>,
}

impl MessageListParams {
    /// 상한을 적용한 실제 개수.
    pub fn effective_limit(&self) -> u32 {
        self.limit
            .unwrap_or(MESSAGE_LIST_DEFAULT)
            .clamp(1, MESSAGE_LIST_MAX)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageListResult {
    pub messages: Vec<MessageView>,
    #[serde(default)]
    pub gaps: Vec<GapView>,
}

/// 보낼 첨부(데몬이 읽을 로컬 파일 경로, 또는 `blob.paste`로 미리 올린 붙여넣기).
///
/// 동영상·음성은 데몬이 썸네일·길이를 만들 수 없을 때가 많아 앱이 만든 것을 함께 넘길 수 있습니다
/// (`thumbnail_base64`·`width`·`height`·`duration_ms`). 데몬은 형식·크기를 검사하고, 파일 내용을 동영상·음성으로
/// 가리지 못하면 힌트만 버리고 보냅니다. 이미지는 이 값들을 무시하고 데몬이 직접 썸네일을 만듭니다. 휘발성 채널은
/// 힌트를 쓰지 않습니다.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AttachmentInput {
    /// 절대 경로. `paste`를 주면 비웁니다.
    #[serde(default)]
    pub path: String,
    /// `blob.paste`가 돌려준 붙여넣기 ID(경로 대신). 이때는 `name`이 꼭 있어야 합니다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paste: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 앱이 만든 썸네일(PNG·JPEG·WebP, 64KiB·긴 변 640px 이하) base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail_base64: Option<String>,
    /// 동영상 가로·세로(픽셀). 둘을 함께 줍니다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// 재생 길이(밀리초). 데몬이 파일에서 길이를 읽을 수 있으면(MP4) 그 값을 씁니다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// 참이면 메타데이터(촬영 위치·기기·시각 등)를 지우지 않고 파일 그대로 보냅니다. 기본은 지웁니다.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keep_metadata: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageSendParams {
    pub space: SpaceId,
    pub channel: ChannelId,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<EventId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<EventId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageEditParams {
    pub space: SpaceId,
    pub channel: ChannelId,
    pub id: EventId,
    pub text: String,
}

/// `{space, channel, id}`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageRefParams {
    pub space: SpaceId,
    pub channel: ChannelId,
    pub id: EventId,
}

/// `emoji`는 유니코드 또는 `<:이름:ID>`·`<a:이름:ID>`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactParams {
    pub space: SpaceId,
    pub channel: ChannelId,
    pub id: EventId,
    pub emoji: String,
}

/// 일반 등급만 검색합니다(로컬 DB).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageSearchParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space: Option<SpaceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<ChannelId>,
    /// 이 사람이 쓴 글만(익명 글은 빼고). 주면 `query`가 비어도 됩니다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<UserId>,
    /// 빈칸으로 나눈 낱말이 모두 든 글(큰따옴표로 묶으면 구절 그대로). 대소문자·전각 무시.
    #[serde(default)]
    pub query: String,
    /// 최신순 결과에서 건너뛸 수. 끝을 넘으면 마지막 쪽.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
    /// 쪽 크기. 기본 50, 최대 500.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForumCreatePostParams {
    pub space: SpaceId,
    pub channel: ChannelId,
    pub title: String,
    #[serde(default)]
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<u32>,
    /// `message.send`와 같은 첨부(`attach_files`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentInput>,
    /// 올리자마자 대문으로(그 채널의 `manage_channels`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub featured: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForumListPostsParams {
    pub space: SpaceId,
    pub channel: ChannelId,
    #[serde(default)]
    pub sort: super::types::ForumSort,
    #[serde(default)]
    pub order: super::types::SortOrder,
    /// 태그 하나로 거르기(예전 호출). `tags`와 함께 주면 `tags`에 더합니다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<u32>,
    #[serde(default)]
    pub tag_match: super::types::TagMatch,
}

/// `forum.get_post`: 게시글 하나(목록 항목과 같은 모양).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForumPostParams {
    pub space: SpaceId,
    pub channel: ChannelId,
    pub post: EventId,
}

/// `forum.set_featured`: 대문 게시글(`null`이면 대문 없음).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForumFeaturedParams {
    pub space: SpaceId,
    pub channel: ChannelId,
    pub post: Option<EventId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForumUpdatePostParams {
    pub space: SpaceId,
    pub channel: ChannelId,
    pub post: EventId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<u32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked: Option<bool>,
}

// ───────────── 첨부·이모지·블롭 ─────────────

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobParams {
    pub space: SpaceId,
    pub blob: BlobId,
}

/// `library.list`의 쪽 크기 기본값·상한, `messages`로 고를 수 있는 메시지 수 상한.
pub const LIBRARY_PAGE_MAX: u32 = 500;
pub const LIBRARY_PICK_MAX: usize = 200;

/// `library.list`: 첨부를 실은 메시지(라이브러리 탭). `messages`를 주면 그 메시지만 썸네일과 함께,
/// 없으면 전체를 쪽으로 나눠 썸네일 없이 돌려줍니다(쪽 하나가 프레임 상한을 넘지 않게).
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LibraryListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space: Option<SpaceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<EventId>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// `blob.relink`: 이 기기에 데이터가 없는 첨부를 같은 내용의 파일로 다시 잇습니다(일반 채널만).
/// `blob.paste`: 붙여넣은 바이트를 RAM에 미리 올립니다. 처음 부를 때(`paste` 없음) 전체 크기(`size`)를 주고, 같은
/// ID로 나머지 조각을 이어 붙입니다(조각 하나는 4MiB 이하).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobPasteParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paste: Option<String>,
    /// 전체 바이트 수(처음 부를 때만).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    pub data_base64: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobPasteResult {
    pub paste: String,
    /// 지금까지 받은 바이트 수.
    pub received: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobPasteDiscardParams {
    pub paste: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRelinkParams {
    pub space: SpaceId,
    pub channel: ChannelId,
    /// 첨부가 달린 메시지(포럼 게시글 포함).
    pub message: EventId,
    pub blob: BlobId,
    /// 절대 경로.
    pub path: String,
}

/// `rate.set_limit`: 그 범위의 제한을 통째로 바꿉니다. 둘 다 `null`이면 그 범위의 제한을 지웁니다. 0은 그 방향으로
/// 아무것도 보내거나 받지 않음입니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitParams {
    pub scope: super::types::RateScope,
    #[serde(default)]
    pub up_bps: Option<u64>,
    #[serde(default)]
    pub down_bps: Option<u64>,
}

/// `blob.cache_set_limit`: 빠진 필드는 그대로 둡니다. `reset`이면 두 값 모두 기본값(데몬 시작 옵션)으로.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BlobCacheLimitParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_free_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reset: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobFetchParams {
    pub space: SpaceId,
    pub blob: BlobId,
    /// 클수록 먼저.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<u8>,
}

/// 블롭 읽기. `key_from`은 그 첨부를 실은 메시지(또는 이모지 추가) 이벤트.
///
/// `index`를 주면 조각 하나, 아니면 `offset`·`len` 범위(최대 [`BLOB_READ_MAX_LEN`]).
/// `binary: true`면 결과가 JSON 대신 바이너리 프레임으로 옵니다(요청 ID가 음이 아닌 정수여야 함).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobReadParams {
    pub space: SpaceId,
    pub blob: BlobId,
    pub key_from: EventId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub len: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobReadResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_base64: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobSaveParams {
    pub space: SpaceId,
    pub blob: BlobId,
    pub key_from: EventId,
    pub path: String,
    /// 휘발성 채널 첨부 저장 확인. 저장한 파일은 메시지가 사라져도 남으므로, 휘발성 첨부는 사람 토큰이 이 값을
    /// 켰을 때만 저장합니다(꺼져 있으면 `1010`). 일반 첨부에는 영향이 없습니다.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_volatile: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmojiCreateParams {
    pub space: SpaceId,
    pub name: String,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmojiCreateResult {
    #[serde(with = "emoji_id_str")]
    pub id: EmojiId,
    pub animated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmojiDeleteParams {
    pub space: SpaceId,
    #[serde(with = "emoji_id_str")]
    pub id: EmojiId,
}

// ───────────── 역할·초대 ─────────────

/// `permissions`는 권한 이름(`manage_channels`, `kick` …, `yangpa_core::model::Permissions::NAMES`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleCreateParams {
    pub space: SpaceId,
    pub name: String,
    /// 0xRRGGBB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<u32>,
    pub permissions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleRef {
    pub role: RoleId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleUpdateParams {
    pub space: SpaceId,
    pub role: RoleId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank: Option<u32>,
    /// 이 역할을 볼 수 있는 역할. 빠지면 그대로, `null`이면 모든 멤버, 목록이면 그 역할을 가진 사람만(역할을 가진
    /// 사람과 역할 관리자는 늘 봄). @everyone에는 쓸 수 없습니다. 화면에서만 숨깁니다.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub visible_to: Option<Option<Vec<RoleId>>>,
    /// 이 역할을 가진 사람이 관리자가 누구인지 보는지(기본 켬, 멤버의 역할 중 하나라도 켜져 있으면 봄).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub see_admins: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleParams {
    pub space: SpaceId,
    pub role: RoleId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteCreateParams {
    pub space: SpaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_uses: Option<u32>,
    /// 가입 시 줄 역할.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<RoleId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteRevokeParams {
    pub space: SpaceId,
    pub token_hash: TokenHash,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InviteJoinStatus {
    /// 확정 기기가 가입 기기를 넣은 크루 키 갱신을 낼 때까지 대기.
    Pending,
    Joined,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteJoinResult {
    pub space: SpaceId,
    pub status: InviteJoinStatus,
}

// ───────────── 동기화 설정 ─────────────

/// 빠진 필드는 그대로 둡니다.
///
/// - `channels`: `null` = 기본 범위(본인이 연 채널만), 목록 = 정확히 그 채널들.
/// - `upload_limit_bps`: `null` = 무제한, 0 = 올리지 않음(남에게 내주지 않음).
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SeedSetParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space: Option<SpaceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub types: Option<Vec<DataType>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub channels: Option<Option<Vec<ChannelId>>>,
    /// 기본 범위일 때 열지 않은 채널까지 제공(docs/api.md 보충).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub all_channels: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_channels: Option<Vec<ChannelId>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub upload_limit_bps: Option<Option<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<u32>,
    /// 제공 범위 채널의 첨부를 열지 않아도 미리 받아 둠(헤드리스 시더).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefetch: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeedExcludeParams {
    pub space: SpaceId,
    pub channel: ChannelId,
    pub id: EventId,
    pub excluded: bool,
}

/// 빠진 필드는 그대로 둡니다. `download_limit_bps: null` = 무제한, 0 = 첨부를 받지 않음.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PeerSetParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space: Option<SpaceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub types: Option<Vec<DataType>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priorities: Option<BTreeMap<DataType, u8>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub download_limit_bps: Option<Option<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_download_max_bytes: Option<u64>,
}

// ───────────── 네트워크 ─────────────

/// `net.set_stun_servers`: `host:port` 목록(앞의 `stun:`은 떼고 포트가 없으면 3478). `null`이면 기본 목록으로,
/// 빈 목록이면 STUN을 쓰지 않습니다.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StunServersParams {
    #[serde(default)]
    pub servers: Option<Vec<String>>,
}

/// `crew.set_servers`: 크루 STUN 서버 목록 전체(`manage_space`). 줄 꼴은 `net.set_stun_servers`와 같고, 8줄까지.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrewServersParams {
    pub space: SpaceId,
    #[serde(default)]
    pub stun: Vec<String>,
}

/// `crew.use_servers`: 이 기기가 그 크루의 STUN 서버를 쓸지(이 기기에만 저장).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrewUseServersParams {
    pub space: SpaceId,
    pub enabled: bool,
}

/// `net.set_bridges`.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BridgesParams {
    pub enabled: bool,
    #[serde(default)]
    pub kind: BridgeKind,
    /// 직접 넣은 브리지 줄(빈 줄은 빼고 [`MAX_CUSTOM_BRIDGES`]줄까지). 다른 종류를 골라도 저장해 둡니다.
    #[serde(default)]
    pub custom: Vec<String>,
}

/// `net.set_file_circuits`.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ChannelTopicsParams {
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileCircuitsParams {
    /// 예전 호출: 켜면 4개, 끄면 0개. `count`를 주면 무시합니다.
    #[serde(default)]
    pub enabled: bool,
    /// 상대 하나에 더 여는 파일 회로 수(0~4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u8>,
}

// ───────────── 음성 ─────────────

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceJoinResult {
    pub call: CallId,
    pub participants: Vec<VoiceParticipant>,
    /// 프레임 E2EE 키(base64). 화면에만 쓰고 저장·로그 금지.
    pub media_key: SecretString,
    pub generation: u64,
    pub topology: Topology,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sfu_host: Option<DeviceId>,
    /// SFU 호스트 기기의 사용자.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sfu_host_user: Option<UserId>,
    /// 이 통화에 쓸 STUN 서버(내 것 먼저, 그다음 쓰기로 한 크루 것).
    #[serde(default)]
    pub ice_servers: Vec<IceServer>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceSignalParams {
    pub space: SpaceId,
    pub channel: ChannelId,
    pub call: CallId,
    pub to: DeviceId,
    pub payload: SignalPayload,
}

/// Tor 통화의 소리 패킷 하나(`voice.media`). 앱이 통화 미디어 키로 봉인한 바이트를 base64로. 데몬은 내용을 보지 않고
/// 지금 참가자 모두에게 Tor 연결로 나릅니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceMediaParams {
    pub space: SpaceId,
    pub channel: ChannelId,
    pub call: CallId,
    pub data: String,
    /// 받을 참가자(없으면 모두). 화면 조각은 '화면 보기'를 누른 사람에게만 보냅니다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<Vec<DeviceId>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceStateParams {
    pub space: SpaceId,
    pub channel: ChannelId,
    pub muted: bool,
    pub deafened: bool,
    pub screen: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceWatchParams {
    pub space: SpaceId,
    pub channel: ChannelId,
    /// 화면공유 중인 기기.
    pub device: DeviceId,
    pub watching: bool,
}

// ───────────── 토큰·감사·사람 확인 ─────────────

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCreateParams {
    pub name: String,
    pub scopes: Vec<Scope>,
    /// 비면 전체 공간.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spaces: Vec<SpaceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation: Option<bool>,
}

/// 토큰 원문은 이때 한 번만 나옵니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCreateResult {
    pub token: SecretString,
    /// 폐기(`token.revoke`)에 쓰는 ID(docs/api.md 보충).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AuditListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// 이 ID보다 앞(오래된 쪽)만.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConfirmApproveResult {
    /// 승인으로 실행된 원래 호출의 결과.
    pub result: Value,
}

// ───────────── 이벤트 구독 ─────────────

/// `topics`가 비면 전부. `message`처럼 앞부분만 주면 `message.*` 전부.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SubscribeParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topics: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscribeResult {
    pub subscription: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnsubscribeParams {
    pub subscription: u64,
}

// ───────────── 모더레이션 ─────────────

/// `member.ban`. `delete_recent`면 그 사용자가 최근 24시간 동안 쓴 메시지도 멤버 노드마다 지웁니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberBanParams {
    pub space: SpaceId,
    pub user: UserId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub delete_recent: bool,
}

/// `message.report`. `hide`면 이 기기에서 그 메시지를 숨기고 시딩에서도 뺍니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageReportParams {
    pub space: SpaceId,
    pub channel: ChannelId,
    pub message: EventId,
    pub reason: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hide: bool,
}

/// `report.list`. `status`가 없으면 전부.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportListParams {
    pub space: SpaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ReportStatus>,
}

/// `report.resolve`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportResolveParams {
    pub space: SpaceId,
    pub report_id: String,
    pub action: ReportAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

// ───────────── 보관 전용 노드 ─────────────

/// `archive.offer`(보관 노드에서). `name`은 관리자 화면에 보일 이름 제안.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveOfferParams {
    /// 링크 유효 시간(초). 기본 24시간, 최대 30일.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `archive.offer` 결과: 관리자에게 따로(안전한 경로로) 전할 1회용 링크.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveOfferResult {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
}

/// `archive.designate`. 공개 채널은 늘 보관하고, 비공개 채널은 `private_channels`에 고른 것만.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveDesignateParams {
    pub space: SpaceId,
    /// 보관 노드의 `archive.offer` 링크.
    pub url: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub private_channels: Vec<ChannelId>,
    /// 표시 이름. 없으면 링크의 제안 이름.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `archive.revoke`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveRevokeParams {
    pub space: SpaceId,
    pub device: DeviceId,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn channel_create_matches_doc_shape() {
        let space = SpaceId([1; 16]);
        let p: ChannelCreateParams = serde_json::from_value(json!({
            "space": space.to_string(),
            "name": "비밀",
            "kind": "text",
            "security": "volatile",
            "join_mode": "join_leave",
            "ttl_secs": 30,
            "private_roles": [],
        }))
        .unwrap();
        assert_eq!(p.kind, ChannelKind::Text);
        assert_eq!(p.security, Some(SecurityLevel::Volatile));
        assert_eq!(p.join_mode, Some(JoinMode::JoinLeave));
        assert_eq!(p.ttl_secs, Some(30));
        assert_eq!(p.private_roles, Some(vec![]));
        assert_eq!(p.p2p, None);
        let v = serde_json::to_value(&p).unwrap();
        assert!(v.get("parent").is_none());
        assert!(v.get("p2p").is_none());
        assert_eq!(v["kind"], "text");
        let p: ChannelCreateParams = serde_json::from_value(json!({
            "space": space.to_string(),
            "name": "직접",
            "kind": "voice",
            "p2p": true,
        }))
        .unwrap();
        assert_eq!(p.p2p, Some(true));
    }

    #[test]
    fn message_list_limit_is_clamped() {
        let mut p = MessageListParams {
            space: SpaceId([1; 16]),
            channel: ChannelId([2; 16]),
            before: None,
            limit: Some(10_000),
            thread: None,
        };
        assert_eq!(p.effective_limit(), MESSAGE_LIST_MAX);
        p.limit = None;
        assert_eq!(p.effective_limit(), MESSAGE_LIST_DEFAULT);
        p.limit = Some(0);
        assert_eq!(p.effective_limit(), 1);
    }

    #[test]
    fn seed_set_three_state_fields() {
        let p: SeedSetParams = serde_json::from_value(json!({
            "types": ["text", "emoji"],
            "channels": null,
            "upload_limit_bps": 2097152,
        }))
        .unwrap();
        assert_eq!(p.types, Some(vec![DataType::Text, DataType::Emoji]));
        assert_eq!(p.channels, Some(None));
        assert_eq!(p.upload_limit_bps, Some(Some(2_097_152)));
        assert_eq!(p.space, None);
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(
            v,
            json!({"types": ["text", "emoji"], "channels": null, "upload_limit_bps": 2097152})
        );
    }

    #[test]
    fn secrets_are_redacted_in_debug() {
        let p = HelloParams {
            token: "s3cret".into(),
        };
        assert!(!format!("{p:?}").contains("s3cret"));
        let r = DeviceLinkJoinParams {
            url: "yangpa://device/s3cret".into(),
        };
        assert!(!format!("{r:?}").contains("s3cret"));
        assert_eq!(
            serde_json::to_value(&p).unwrap(),
            json!({"token": "s3cret"})
        );
    }

    #[test]
    fn invite_and_emoji_shapes() {
        let p = InviteCreateParams {
            space: SpaceId([3; 16]),
            expires_in_secs: Some(86_400),
            max_uses: Some(10),
            roles: vec![],
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(
            v,
            json!({"space": SpaceId([3; 16]).to_string(), "expires_in_secs": 86400, "max_uses": 10})
        );
        let e = EmojiCreateResult {
            id: EmojiId(12345678901234567890),
            animated: true,
        };
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            json!({"id": "12345678901234567890", "animated": true})
        );
    }

    #[test]
    fn voice_signal_payload_shape() {
        let p = VoiceSignalParams {
            space: SpaceId([1; 16]),
            channel: ChannelId([2; 16]),
            call: CallId([3; 16]),
            to: DeviceId([4; 32]),
            payload: SignalPayload::Offer { sdp: "v=0".into() },
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["payload"], json!({"type": "offer", "sdp": "v=0"}));
        assert_eq!(serde_json::from_value::<VoiceSignalParams>(v).unwrap(), p);
    }
}
