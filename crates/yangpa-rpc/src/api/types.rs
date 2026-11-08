//! API 보기 타입과 설정 DTO.
//!
//! 데몬 내부 타입(`yangpa-sync`의 정책, `yangpa-net`의 상태)에 의존하지 않도록 같은 JSON 모양의 DTO를 둡니다.
//! 필드 이름이 내부 타입의 serde 이름과 같아서 데몬은 `serde_json` 왕복으로도 바꿀 수 있습니다.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use yangpa_core::badge::Badge;
use yangpa_core::device::{DeviceEntry, Profile};
use yangpa_core::ids::{BlobId, ChannelId, DeviceId, EmojiId, EventId, RoleId, SpaceId, UserId};
use yangpa_core::model::{
    ChannelSettings, EmojiLimits, Role, RoleDisplay, SpaceKind, SpaceSettings,
};
use yangpa_core::time::Timestamp;

use super::Scope;
use super::serde_util::{TokenHash, emoji_id_str};

/// 익명 글·참가자의 작성자 자리에 넣는 사용자 ID(모두 0). 아무도 이 공개 키의 비밀 키를 갖지 않습니다.
pub const ANONYMOUS_USER: UserId = UserId::from_bytes([0; 32]);
/// 익명 글·참가자의 기기 자리(모두 0).
pub const ANONYMOUS_DEVICE: DeviceId = DeviceId::from_bytes([0; 32]);

/// 메시지 보기.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageView {
    pub id: EventId,
    pub author: UserId,
    pub author_device: DeviceId,
    pub created_at: Timestamp,
    pub lamport: u64,
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<AttachmentView>,
    #[serde(default)]
    pub embeds: Vec<EmbedView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<EventId>,
    #[serde(default)]
    pub reactions: Vec<ReactionView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_at: Option<Timestamp>,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub volatile: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
    /// 포럼 게시글 안 댓글이면 게시글 ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<EventId>,
    /// `message.search`처럼 여러 채널에 걸친 결과에서만 채웁니다(docs/api.md 보충).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space: Option<SpaceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<ChannelId>,
    /// 내가 차단한 사용자의 메시지. 내용은 그대로 두고 화면이 접어서 보여 줍니다.
    #[serde(default)]
    pub author_blocked: bool,
    /// 익명 글: 익명 채널에서 썼거나 그 채널이 지금 익명입니다. `author`·`author_device`는 [`ANONYMOUS_USER`]·
    /// [`ANONYMOUS_DEVICE`]이고, `author_blocked`는 늘 거짓입니다.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub anonymous: bool,
    /// 익명 글의 가명('형용사 + 동물', 같은 채널에서 같은 사람은 늘 같은 이름). 익명이 아니면 없음.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anon_name: Option<String>,
    /// 내가 쓴 글(익명 글도 내 글이면 참).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mine: bool,
    /// 나를 멘션한 글: `<@내 ID>`, 또는 그 채널에서 `mention_many` 권한이 있는 사람의 `@everyone`, 작성 뒤 5분 안에 받은
    /// `@here`. 받는 쪽 데몬이 작성자 권한을 보고 정합니다.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mentions_me: bool,
}

impl MessageView {
    /// 내용(본문·첨부·미리보기·반응)을 지운 사본. 자동화 토큰에 휘발성 내용을 넘기지 않을 때 씁니다.
    pub fn redacted(&self) -> MessageView {
        MessageView {
            text: String::new(),
            attachments: Vec::new(),
            embeds: Vec::new(),
            reactions: Vec::new(),
            ..self.clone()
        }
    }
}

/// 첨부 보기. 복호화 키는 노출하지 않고, 원본은 `blob.read({key_from: 메시지 ID})`로 읽습니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentView {
    pub blob: BlobId,
    pub name: String,
    pub mime: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// 보내는 쪽이 만든 썸네일(base64).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail_base64: Option<String>,
    /// 원본을 모두 받았는지.
    #[serde(default)]
    pub downloaded: bool,
}

/// 링크 미리보기 보기(보내는 쪽이 만든 것).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbedView {
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub site_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail_base64: Option<String>,
    /// 눌러야 불러오는 미디어 플레이어 주소.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub player_url: Option<String>,
}

/// 반응 집계. `emoji`는 유니코드 또는 `<:이름:ID>`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionView {
    pub emoji: String,
    pub count: u32,
    pub me: bool,
}

/// 받지 않은 기록 구간.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GapView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<EventId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<EventId>,
}

/// `space.list` 항목.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceSummary {
    pub space: SpaceId,
    pub kind: SpaceKind,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<BlobId>,
    /// 읽지 않은 메시지 수.
    #[serde(default)]
    pub unread: u32,
    /// 읽지 않은 멘션 수.
    #[serde(default)]
    pub mention: u32,
    /// 휴면 공간(주기 동기화 안 함).
    #[serde(default)]
    pub dormant: bool,
    #[serde(default)]
    pub member_count: u32,
}

/// `space.get`: 해소된 공간 상태.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpaceView {
    pub space: SpaceId,
    pub kind: SpaceKind,
    pub settings: SpaceSettings,
    pub owner: UserId,
    /// 키 그룹 갱신(`KeyUpdate`)을 내는 확정 기기.
    pub committer: DeviceId,
    #[serde(default)]
    pub members: Vec<MemberView>,
    #[serde(default)]
    pub roles: Vec<Role>,
    /// 채널 트리(`settings.parent`로 묶음, `settings.position` 순).
    #[serde(default)]
    pub channels: Vec<ChannelView>,
    #[serde(default)]
    pub emojis: Vec<EmojiView>,
    /// 크루 커스텀 이모지 한도(`crew.update`의 `emoji_limits`). 비면 개수 500까지, 크기 무제한.
    #[serde(default)]
    pub emoji_limits: EmojiLimits,
    /// 이 공간에서 내 권한 이름(`manage_channels` …). 소유자는 전부.
    #[serde(default)]
    pub my_permissions: Vec<String>,
    /// 확정 기기가 오프라인이라 멤버·기기 변경이 아직 키 그룹에 반영되지 않음.
    #[serde(default)]
    pub pending_commit: bool,
    /// 나간·내보낸 기기를 뺀 키 갱신을 기다리다(`rotation_wait`) 옛 키로 보냈고 아직 갱신이 오지 않음. 그 사이 보낸 글은
    /// 나간 기기도 읽을 수 있습니다(앱이 "키 회전 대기 중"을 표시).
    #[serde(default)]
    pub key_rotation_pending: bool,
    /// 1대1 → 크루 전환을 요청한 사용자(상대 동의 대기).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_crew_conversion: Option<UserId>,
    #[serde(default)]
    pub dormant: bool,
    /// 이 기기가 이 크루를 신뢰하는지(크루 멤버에게 이 기기의 IP를 보여도 됨, `trust.set_space`).
    #[serde(default)]
    pub trusted: bool,
    /// 역할별 표시 설정(기본값이 아닌 것만). 역할을 관리할 수 있는 사람에게만 채웁니다.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub role_display: BTreeMap<RoleId, RoleDisplay>,
    /// 내가 관리자가 누구인지 볼 수 없음. 관리자 권한 역할은 `roles`와 멤버의 `roles`에서 이미 뺐고, 앱은 소유자
    /// 표시도 숨깁니다.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub admins_hidden: bool,
    /// 크루 STUN 서버 목록(고칠 수 있는 사람, `manage_space`에게만).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crew_servers: Option<CrewServers>,
    /// 크루 STUN 서버가 있고 내 역할이 쓸 수 있음(`use_crew_stun`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub crew_stun: bool,
    /// 이 기기가 크루 서버를 쓰기로 했음(`crew.use_servers`). 권한이 있을 때(`crew_stun`)만 씁니다.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub use_crew_servers: bool,
    /// 소유자가 해산한 크루(`crew.disband`). 아무도 쓸 수 없고 `crew.leave`로 이 기기에서 지웁니다.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disbanded: bool,
}

/// 채널 보기.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelView {
    pub channel: ChannelId,
    pub settings: ChannelSettings,
    #[serde(default)]
    pub unread: u32,
    /// 입장·퇴장 방식 채널에 내가 들어가 있는지.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub joined: Option<bool>,
    /// 입장·퇴장 방식·통화 채널의 현재 참가자.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub participants: Option<Vec<UserId>>,
    /// 휘발성 채널의 API(자동화 토큰) 읽기 허용 여부.
    #[serde(default)]
    pub api_read_allowed: bool,
    /// P2P 전용 채널(내용을 참여 기기끼리 직접 주고받아 서로 IP가 보임).
    #[serde(default)]
    pub p2p: bool,
    /// 이 기기가 경고를 받아들이고 그 P2P 전용 채널에 참여 중인지(`channel.p2p_join`). 아니면 채널이 잠겨 있습니다.
    #[serde(default)]
    pub p2p_joined: bool,
    /// 익명 채널. 글쓴이와 `participants`가 가려집니다(참가자는 수만 맞춰 [`ANONYMOUS_USER`]로 채움).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub anonymous: bool,
    /// 포럼 채널의 대문 게시글(`forum.set_featured`). 이 기기에 없거나 지운 글이면 비웁니다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub featured_post: Option<EventId>,
    /// 첨부 하나의 최대 바이트(크루 관리자가 채널마다 정함). 없으면 무제한, 0이면 첨부 금지.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach_limit: Option<u64>,
    /// 수정 금지 채널(아무도 메시지를 고칠 수 없음).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_edit: bool,
    /// 삭제 금지 채널(작성자가 자기 메시지를 지울 수 없음, 운영 삭제는 됨).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_delete: bool,
    /// 상시 휘발성 채널의 참가 기기 상한.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volatile_cap: Option<u32>,
    /// 상한이 차서 이 기기가 그 상시 휘발성 채널에 참가하지 못하는 중(자리가 나면 자동으로 참가).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub volatile_full: bool,
    /// 채널 토픽을 쓰는 큰 크루에서 따라가지 않는 이 채널에 이 기기가 아직 받지 않은 새 글이 있음(멤버가 알려 준 채널
    /// 끝 기준, 열면 받아 옴).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_new: bool,
}

/// 멤버 보기.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberView {
    pub user: UserId,
    pub profile: Profile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    #[serde(default)]
    pub roles: Vec<RoleId>,
    /// 화면에 보일 접속 상태(숨기 중인 기기는 빼고, 나는 내 표시 상태).
    #[serde(default)]
    pub online: bool,
    /// 온라인이지만 자리 비움.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub away: bool,
    /// 자동화(AI)가 이 멤버로 동작 중.
    #[serde(default)]
    pub automation: bool,
    /// 내가 차단한 사용자(이 기기에만 있는 정보).
    #[serde(default)]
    pub blocked: bool,
    /// 이 기기와 마지막으로 동기화한 시각(ms, 보이는 상태로 이어져 있던 마지막 때). 상대가 비공개로 했거나 기록이
    /// 없으면 없음.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<u64>,
}

/// `member.search` 한 쪽.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemberPage {
    pub members: Vec<MemberView>,
    /// 조건에 맞는 멤버 수(쪽과 무관).
    pub total: u32,
    /// 지금 온라인인 멤버 수(조건과 무관).
    pub online: u32,
}

/// `message.search` 한 쪽.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageSearchPage {
    /// 이 쪽의 글(최신순). `space`·`channel`을 채웁니다.
    pub messages: Vec<MessageView>,
    /// 조건에 맞는 전체 글 수(쪽과 무관, 상한 없음).
    pub total: u32,
    /// 이 쪽의 실제 시작 위치(요청한 `offset`이 끝을 넘으면 마지막 쪽).
    pub offset: u32,
    /// 데몬에서 검색에 걸린 시간(ms). 첫 검색은 색인을 만드느라 깁니다.
    pub took_ms: u32,
}

/// 커스텀 이모지.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmojiView {
    #[serde(with = "emoji_id_str")]
    pub id: EmojiId,
    pub name: String,
    pub animated: bool,
    pub blob: BlobId,
    #[serde(default)]
    pub mime: String,
    /// 올린 사용자. 이 판 전의 상태 캐시에서 온 이모지면 없을 수 있습니다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<UserId>,
}

/// 초대 보기.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteView {
    pub token_hash: TokenHash,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_uses: Option<u32>,
    #[serde(default)]
    pub uses: u32,
    #[serde(default)]
    pub roles: Vec<RoleId>,
    pub created_by: UserId,
}

/// 포럼 게시글 정렬.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForumSort {
    /// 최근 활동순.
    #[default]
    Activity,
    /// 작성일순.
    Created,
}

/// 포럼 게시글 정렬 방향.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    /// 새것(큰 값)부터.
    #[default]
    Desc,
    /// 옛것(작은 값)부터.
    Asc,
}

/// 여러 태그로 거를 때 맞추는 방식.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagMatch {
    /// 고른 태그 중 하나라도 있는 글.
    #[default]
    Any,
    /// 글의 태그가 고른 태그와 꼭 같은 글(더 붙은 태그도 없음).
    All,
}

/// 포럼 게시글 목록 항목. 본문·댓글은 게시글을 열 때(`message.list({thread})`) 받습니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForumPostView {
    pub id: EventId,
    pub title: String,
    #[serde(default)]
    pub tags: Vec<u32>,
    pub author: UserId,
    pub created_at: Timestamp,
    pub last_activity: Timestamp,
    #[serde(default)]
    pub reply_count: u32,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub locked: bool,
    /// 익명 게시글(`author`는 [`ANONYMOUS_USER`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub anonymous: bool,
    /// 익명 게시글의 가명(`MessageView::anon_name`과 같음).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anon_name: Option<String>,
    /// 내가 쓴 게시글.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mine: bool,
    /// 이 채널의 대문 게시글.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub featured: bool,
    /// 본문을 마지막으로 고친 때(수정 페이지에서 저장하면 제목·태그만 바꿔도 함께 정해짐). 고친 적 없으면 없음.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_at: Option<Timestamp>,
    /// 본문 앞부분(마크다운 그대로, 줄바꿈 포함, [`FORUM_PREVIEW_CHARS`]자까지).
    #[serde(default)]
    pub preview: String,
    /// 갤러리 보기의 표지: 게시글 첨부 중 첫 이미지·영상.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover: Option<AttachmentView>,
    /// 게시글 첨부 수.
    #[serde(default)]
    pub attachment_count: u32,
}

/// [`ForumPostView::preview`] 길이(글자).
pub const FORUM_PREVIEW_CHARS: usize = 200;

/// 주 스웜의 종류. 앱 사용자 기기는 늘 `tor`이고, `normal`은 직접 연결 주 스웜(`--direct`: 상시 노드·시험)입니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetMode {
    #[default]
    Normal,
    Tor,
}

/// NAT 상태.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NatStatus {
    #[default]
    Unknown,
    Public,
    Private,
    /// Tor 스웜이라 검사하지 않음.
    Disabled,
}

/// `net.status`. 주소는 multiaddr 문자열.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NetStatus {
    pub peer_id: String,
    pub mode: NetMode,
    /// 이 기기의 수신 주소(내부 IP가 있으므로 진단용으로만).
    #[serde(default)]
    pub listen_addrs: Vec<String>,
    /// 릴레이 경유 주소.
    #[serde(default)]
    pub relay_addrs: Vec<String>,
    #[serde(default)]
    pub nat: NatStatus,
    #[serde(default)]
    pub connections: u32,
    /// Tor 부트스트랩 진행률(0~100, 100은 회로를 만들 수 있을 때만).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap_progress: Option<u8>,
    // 이하 docs/api.md 보충 필드.
    #[serde(default)]
    pub external_addrs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onion_addr: Option<String>,
    #[serde(default)]
    pub peers: u32,
    /// 시작할 때 다시 거는 연결(인증까지)이 아직 끝나지 않은 친구·나(내 다른 기기). 끝날 때마다 `net.status`로 알림.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connecting: Vec<UserId>,
    /// 오프라인 모드(`presence.set`): 이 데몬은 망 없이 떠 있어 Tor도 켜지지 않습니다.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub offline: bool,
}

/// 이 기기의 표시 상태 고르기. `online`은 자동(입력이 한동안 없으면 자리 비움)이고 기본값입니다.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresenceMode {
    #[default]
    Online,
    Away,
    /// 숨기: 동기화·대화는 그대로, 다른 사람 화면에는 오프라인.
    Invisible,
    /// 오프라인: 데몬이 망(Tor 포함) 없이 다시 뜹니다.
    Offline,
}

/// `badge.get`·`badge.set_shown`. 원년멤버는 개발자 증서가 맞을 때만 들어 있습니다. 원년멤버 말고는 그 사용자의 데몬이
/// 스스로 계산해 알린 값입니다.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BadgeView {
    /// 모은 뱃지(번호 순).
    pub earned: Vec<Badge>,
    /// 이름 앞뒤에 보일 뱃지([`yangpa_core::badge::MAX_SHOWN`]개까지).
    pub shown: Vec<ShownBadgeView>,
    /// 내가 개발자 계정이고 이 사용자에게 아직 원년멤버 증서를 주지 않았음(`badge.grant_founder`).
    #[serde(default)]
    pub can_grant_founder: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShownBadgeView {
    pub badge: Badge,
    /// 이름 뒤(아니면 앞).
    pub after: bool,
}

/// `presence.get`·`presence.set`·`presence.self` 알림.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceView {
    pub mode: PresenceMode,
    /// 이때 `online`으로 돌아갑니다. 없으면 바꿀 때까지.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<Timestamp>,
    /// 앱이 알린 입력 없음(`online`일 때 자리 비움으로 보임).
    #[serde(default)]
    pub idle: bool,
    /// 내 마지막 동기화 시각을 다른 사람 화면에 보이지 않음.
    #[serde(default)]
    pub hide_last_seen: bool,
    /// 이 데몬이 망 없이 떠 있음(오프라인 모드로 시작함).
    #[serde(default)]
    pub offline: bool,
}

/// `flair.list`·`flair.set`·`flair` 알림. 사용자가 낸 후원자 표시를 지금 시각으로 판정한 것입니다. `until`이 지나면
/// 둘 다 꺼진 것으로 봅니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlairView {
    pub user: UserId,
    /// 프로필 카드 배경에 배너를 보임(월 14달러 이상).
    pub banner: bool,
    /// 후광을 쓸 수 있음(월 50달러 이상). 켰는지는 `halo`.
    pub halo_allowed: bool,
    /// 후광 그라데이션의 두 색(0xRRGGBB). 쓸 수 없거나 껐으면 없음.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub halo: Option<[u32; 2]>,
    /// 이 표시를 믿는 끝(밀리초).
    pub until: Timestamp,
}

/// `net.stun_servers` 결과.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StunServers {
    /// `host:port` 목록(앞쪽부터 씀).
    pub servers: Vec<String>,
    /// 저장한 목록이 없어 기본 목록을 쓰는 중.
    #[serde(rename = "default")]
    pub is_default: bool,
}

/// 크루 STUN 서버(`space.get`의 `crew_servers`, `crew.set_servers`).
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CrewServers {
    /// `host:port`.
    #[serde(default)]
    pub stun: Vec<String>,
}

/// `net.file_circuits` 결과.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ChannelTopics {
    /// 큰 크루에서 모두 보는 채널 메시지를 채널 묶음 토픽으로, 멘션·답장은 알림 토픽으로도 주고받음(기본 꺼짐).
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileCircuits {
    /// 큰 첨부를 Tor로 주고받을 때 파일 전용 회로를 씀(`count`가 1 이상, 기본 꺼짐).
    pub enabled: bool,
    /// 상대 하나에 더 여는 파일 회로 수(0~4, 기본 0).
    #[serde(default)]
    pub count: u8,
}

/// 직접 넣을 수 있는 브리지 줄 수.
pub const MAX_CUSTOM_BRIDGES: usize = 32;
/// 브리지 줄 하나의 최대 길이(바이트).
pub const MAX_BRIDGE_LINE: usize = 2048;

/// Tor 브리지 종류.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeKind {
    /// 내장 Snowflake(검열을 만나 스스로 켜는 것도 이것).
    #[default]
    Snowflake,
    /// 내장 obfs4.
    Obfs4,
    /// 직접 넣은 브리지 줄(obfs4·webtunnel·snowflake·meek_lite·일반 브리지).
    Custom,
}

/// `net.bridges` 결과와 `net.bridges` 알림.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BridgesView {
    /// 브리지로 Tor에 들어감(기본 꺼짐).
    pub enabled: bool,
    /// 고른 브리지(꺼져 있어도 남김).
    pub kind: BridgeKind,
    /// 직접 넣은 브리지 줄.
    #[serde(default)]
    pub custom: Vec<String>,
    /// 직접 넣은 브리지 줄(줄바꿈으로 이음)의 QR 코드. 줄이 없거나 QR에 다 들어가지 않으면 없음.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qr: Option<QrCode>,
    /// Tor에 들어가지 못해(검열) 데몬이 Snowflake를 스스로 켰음. 사용자가 바꾸면 꺼짐.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto: bool,
    /// 브리지 전송 프로그램(lyrebird)이 있음. 없으면 켤 수 없습니다.
    pub available: bool,
}

/// Tor 상태.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TorStatus {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap_progress: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onion_addr: Option<String>,
}

/// 시더·피어 설정의 데이터 종류.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataType {
    Text,
    Image,
    Video,
    File,
    Emoji,
    HistoryBundle,
}

impl DataType {
    pub const ALL: [DataType; 6] = [
        DataType::Text,
        DataType::Image,
        DataType::Video,
        DataType::File,
        DataType::Emoji,
        DataType::HistoryBundle,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            DataType::Text => "text",
            DataType::Image => "image",
            DataType::Video => "video",
            DataType::File => "file",
            DataType::Emoji => "emoji",
            DataType::HistoryBundle => "history_bundle",
        }
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for DataType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        DataType::ALL
            .into_iter()
            .find(|t| t.name() == s.trim())
            .ok_or_else(|| {
                format!(
                    "알 수 없는 데이터 종류: {s} (text, image, video, file, emoji, history_bundle)"
                )
            })
    }
}

/// 시더 설정(`yangpa-sync`의 `SeederPolicy`와 같은 JSON 모양).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeederPolicy {
    pub enabled: bool,
    pub types: Vec<DataType>,
    /// `null`이면 기본 범위(본인이 연 채널만, `all_channels`면 볼 수 있는 모든 채널). 목록이면 정확히 그 채널들.
    #[serde(default)]
    pub include_channels: Option<Vec<ChannelId>>,
    #[serde(default)]
    pub all_channels: bool,
    #[serde(default)]
    pub exclude_channels: Vec<ChannelId>,
    /// 개별 메시지 제외.
    #[serde(default)]
    pub excluded_events: Vec<EventId>,
    /// 송신 대역폭 상한(바이트/초). `null`이면 무제한.
    #[serde(default)]
    pub upload_limit_bps: Option<u64>,
    pub max_concurrent: u32,
    /// 제공 범위 채널의 첨부를 열지 않아도 미리 받아 둠(헤드리스 시더, docs/headless.md).
    #[serde(default)]
    pub prefetch: bool,
}

/// 피어 설정(`yangpa-sync`의 `PeerPolicy`와 같은 JSON 모양).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerPolicy {
    pub types: Vec<DataType>,
    /// 종류별 우선순위(클수록 먼저). 없는 종류는 기본값.
    #[serde(default)]
    pub priorities: BTreeMap<DataType, u8>,
    /// 수신 대역폭 상한(바이트/초). `null`이면 무제한.
    #[serde(default)]
    pub download_limit_bps: Option<u64>,
    /// 이 크기 이하의 첨부만 자동으로 받습니다.
    pub auto_download_max_bytes: u64,
}

/// `seed.coverage` 항목. `online_seeders`와 `online_archives`가 모두 0이면 경고 대상.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelCoverage {
    pub channel: ChannelId,
    pub online_seeders: u32,
    /// 그 채널을 제공하는 온라인 보관 전용 노드 수(시더와 따로 셈).
    #[serde(default)]
    pub online_archives: u32,
}

/// API 토큰 보기(토큰 원문은 만들 때 한 번만 나옴).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenView {
    pub id: String,
    pub name: String,
    pub scopes: Vec<Scope>,
    /// 비면 전체 공간.
    #[serde(default)]
    pub spaces: Vec<SpaceId>,
    #[serde(default)]
    pub automation: bool,
    pub created_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<Timestamp>,
}

/// 감사 로그 한 줄(메서드, 토큰 이름, 결과, 시각만. 내용은 남기지 않음).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: u64,
    pub at: Timestamp,
    pub method: String,
    pub token_name: String,
    pub ok: bool,
    /// 실패면 오류 코드.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i64>,
    #[serde(default)]
    pub automation: bool,
}

/// AI가 요청해 사람 확인을 기다리는 파괴적 작업.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingConfirmation {
    pub id: String,
    pub method: String,
    /// 원래 호출 인자(사람이 무엇을 승인하는지 볼 수 있게).
    #[serde(default)]
    pub params: Value,
    /// 요청한 토큰 이름.
    pub token_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space: Option<SpaceId>,
    pub requested_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
    /// 사람이 읽는 요약(예: "운영진 역할 삭제").
    #[serde(default)]
    pub summary: String,
}

/// 통화 참가자.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceParticipant {
    /// 익명 채널이면 [`ANONYMOUS_USER`]이고 `anon`에 번호가 있습니다.
    pub user: UserId,
    /// 통화 연결에 쓰는 기기. 익명 채널에서도 연결하려면 있어야 해서 그대로 둡니다.
    pub device: DeviceId,
    /// 익명 채널의 참가자 번호('익명 1', '익명 2' …, 들어온 순서, 이 통화 동안 유지). 나는 `mine`으로 압니다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anon: Option<u32>,
    /// 익명 채널의 가명(그 채널의 글과 같은 이름, `MessageView::anon_name`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anon_name: Option<String>,
    /// 이 기기의 참가 줄.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mine: bool,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub deafened: bool,
    /// 화면공유 중.
    #[serde(default)]
    pub screen: bool,
    /// 이 기기가 아는 이 참가자 화면의 시청자: 자기 화면을 보는 기기 전부, 남의 화면이면 자기 기기(보는 중일 때).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub viewers: Vec<DeviceId>,
    /// Tor 통화(`Topology::Tor`)에서 이 기기와 서로 신뢰해 WebRTC로 바로 잇는지(서로 IP가 보임). 나머지 참가자와는 Tor로
    /// 소리 패킷을 나릅니다. 거짓이면 생략.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub direct: bool,
}

/// WebRTC ICE 서버(STUN). 브라우저 `RTCIceServer`와 같은 모양.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IceServer {
    /// `stun:` URL.
    pub urls: Vec<String>,
}

/// 통화 토폴로지.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Topology {
    Mesh,
    Sfu,
    /// Tor로 도는 기기의 일반 채널 통화: WebRTC 없이, 앱이 봉인한 소리 패킷을 데몬이 Tor 연결로 나릅니다
    /// (`voice.media` API·알림). 앱은 이 통화에서 RTCPeerConnection을 만들지 않습니다(IP 후보가 모이지 않게).
    Tor,
}

/// 체크포인트 경보(`checkpoint.alert` 알림)의 종류. 운영진이 소유자 기기를 확인할 일입니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointAlertKind {
    /// 같은 순번에 서명이 맞는 다른 체크포인트가 보임(다른 소유자 기기·위임 기기가 따로 냄). 정한 규칙으로 한쪽을
    /// 따르지만, 소유자 기기가 갈라져 있거나 탈취됐을 수 있습니다.
    Conflict,
    /// 같은 기기가 같은 순번에 둘을 씀(갈라 치기). 그 기기가 탈취됐을 수 있습니다.
    Equivocation,
    /// 체크포인트로 들어온 상태가 다른 출처와 다르거나 확인되지 않아, 버리고 처음부터 따라잡는 중.
    Fallback,
}

/// 새 기기의 연결 요청이 어떻게 끝났는지.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceLinkStatus {
    Approved,
    Rejected,
    Expired,
    /// 승인은 됐지만 받은 묶음이 링크와 맞지 않거나 저장하지 못함.
    Failed,
}

/// QR 코드: `size`×`size` 칸, `modules`는 위 줄부터 칸마다 `1`(검정)·`0`(흰색).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QrCode {
    pub size: u32,
    pub modules: String,
}

/// 후원 확인 경로.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SupporterProvider {
    Github,
    Patreon,
}

/// `supporter.status`: 후원자 전용 기능(라이브러리 탭·콘텐츠 뷰어)을 쓸 수 있는지.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupporterStatus {
    /// 지금 쓸 수 있음(정기 후원 확인, 기존 사용자 유예, 또는 확인 경로가 하나도 준비되지 않은 빌드).
    pub active: bool,
    /// 기존 사용자 유예가 끝나는 시각(유예 중일 때만).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace_until: Option<Timestamp>,
    /// 이 빌드에서 쓸 수 있는 확인 경로.
    pub providers: Vec<SupporterProvider>,
    /// 연결한 계정들.
    pub links: Vec<SupporterLink>,
    /// 내 다른 기기가 확인해 보내 준 결과(그 기기에서 연결한 계정). 이 기기는 따로 로그인하지 않아도 됩니다.
    #[serde(default)]
    pub shared: Vec<SupporterShared>,
}

/// 내 다른 기기가 확인해 보내 준 후원 결과.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupporterShared {
    /// 보낸 내 기기.
    pub device: DeviceId,
    pub provider: SupporterProvider,
    pub login: String,
    /// 정기 후원이 확인됐고 아직 유효함.
    pub active: bool,
    pub valid_until: Timestamp,
}

/// 연결한 후원 계정 하나.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupporterLink {
    pub provider: SupporterProvider,
    /// 그 서비스의 계정 이름(GitHub 로그인, Patreon 이름).
    pub login: String,
    /// 정기 후원이 확인됐고 아직 유효함.
    pub active: bool,
    /// 마지막으로 확인한 시각.
    pub checked_at: Timestamp,
    /// 확인 결과를 믿는 마지막 시각(GitHub 3일, Patreon 다음 결제일).
    pub valid_until: Timestamp,
    /// 토큰이 끊겨 다시 로그인해야 함(다른 기기에서 Patreon 로그인, 연결 해제 등).
    pub needs_login: bool,
}

/// `supporter.github_begin`: 사용자가 `verification_uri`에서 넣을 코드. 승인하면 `supporter.updated`가 옵니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupporterDeviceCode {
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in_secs: u64,
}

/// `supporter.patreon_begin`: 브라우저로 열 Patreon 승인 주소. 승인하면 `supporter.updated`가 옵니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupporterAuthUrl {
    pub url: String,
}

/// 친구 보기.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendView {
    pub user: UserId,
    pub profile: Profile,
    pub space: SpaceId,
    /// `MemberView`의 `online`·`away`·`last_seen`과 같습니다.
    #[serde(default)]
    pub online: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub away: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<u64>,
    #[serde(default)]
    pub safety_number_verified: bool,
    /// 친구·임시 연락처(연락처 링크로 만남)·만료된 연락처.
    #[serde(default)]
    pub kind: ContactKind,
    /// 임시 연락처가 만료되는 시각(만료 없음이면 없음).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
    /// 내가 "친구 추가"를 보내고 상대 수락을 기다리는 중.
    #[serde(default)]
    pub proposed: bool,
}

/// `trust.get`: 한 사람에 대한 이 기기의 신뢰와 지금 연결 방식. IP는 싣지 않습니다(앱 어디에도 보이지 않게).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustView {
    pub user: UserId,
    /// 사람 단위 신뢰 스위치(`trust.set_user`).
    pub trusted: bool,
    /// 이 사람이 멤버인 크루 중 내가 신뢰한 크루(크루 신뢰로도 IP가 보임).
    #[serde(default)]
    pub via_crews: Vec<SpaceId>,
    /// 지금 이 사람의 기기와 이어진 방식(가장 빠른 것).
    pub connection: ConnectionMode,
}

/// 한 사람과의 연결 방식.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionMode {
    /// 서로 신뢰(또는 함께 참여한 P2P 전용 채널): 직접 연결이라 서로의 IP가 보임.
    P2p,
    /// 나만 신뢰: 상대가 Tor 출구를 거쳐 내 기기로 바로 걸어 옴(내 IP는 상대에게 보이고 상대는 숨음).
    OneWayMeExposed,
    /// 상대만 신뢰: 내가 Tor 출구를 거쳐 상대 기기로 바로 겁니다(상대 IP가 보이고 나는 숨음).
    OneWayPeerExposed,
    /// onion끼리(양쪽 모두 숨음).
    Tor,
    /// 지금 이어진 기기가 없음.
    #[default]
    Offline,
}

/// 연락처 종류.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContactKind {
    #[default]
    Friend,
    Temporary,
    Expired,
}

/// 받은 친구 요청.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendRequestView {
    pub request_id: String,
    pub user: UserId,
    pub profile: Profile,
    pub received_at: Timestamp,
}

/// 내가 만든 친구 링크.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendLinkView {
    pub token_id: String,
    pub created_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
    #[serde(default)]
    pub single_use: bool,
    #[serde(default)]
    pub uses: u32,
}

/// 연결된 노드.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerView {
    pub peer_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<UserId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<u32>,
    #[serde(default)]
    pub relayed: bool,
}

/// 데몬 진단 수치.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct DiagMetrics {
    pub delivery_ms_p50: u64,
    pub delivery_ms_p95: u64,
    pub memory_bytes: u64,
    pub cpu_percent: f64,
    /// 큐 이름 → 길이.
    #[serde(default)]
    pub queues: BTreeMap<String, u64>,
    pub events_per_sec: f64,
}

/// 블롭 상태. 매니페스트를 아직 받지 못했으면 `total_chunks`가 0입니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobStatusView {
    pub have_chunks: u32,
    pub total_chunks: u32,
    pub seeders_online: u32,
    /// 원본(평문) 크기. 매니페스트가 없으면 메시지에 적힌 크기(docs/api.md 보충).
    #[serde(default)]
    pub size: u64,
    /// 내려받기 대기열에 있거나 받는 중인지.
    #[serde(default)]
    pub downloading: bool,
    /// 사용자가 고정해 저장 공간 정리에서 뺀 블롭(`blob.pin`).
    #[serde(default)]
    pub pinned: bool,
    /// 내가 올린 첨부·이모지·아이콘·프로필 이미지처럼 정리하지 않고 늘 남기는 블롭.
    #[serde(default)]
    pub kept: bool,
    /// 휘발성 채널 첨부(RAM 전용). 파일로 쓰는 클라이언트는 사람에게 확인받아야 합니다(`blob.save`의 `allow_volatile`).
    #[serde(default)]
    pub volatile: bool,
}

/// `blob.cache_status`: 받은 첨부가 쓰는 저장 공간(암호문 조각 바이트). 한도는 정리 대상(`cache_*`)에만 걸립니다.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BlobCacheView {
    /// 정리 대상: 남에게서 받은, 고정하지 않은 첨부.
    pub cache_bytes: u64,
    pub cache_count: u32,
    /// 사용자가 고정한 블롭(`blob.pin`).
    pub pinned_bytes: u64,
    pub pinned_count: u32,
    /// 늘 남기는 블롭: 내가 올린 첨부, 이모지·공간 아이콘·프로필 이미지.
    pub kept_bytes: u64,
    pub kept_count: u32,
    /// 정리 대상 한도(바이트).
    pub limit_bytes: u64,
    /// 데이터 경로 디스크에 남겨 둘 최소 여유 공간(바이트). 이보다 적으면 한도 아래여도 정리합니다.
    pub min_free_bytes: u64,
    /// 설정하지 않았을 때의 값(데몬 시작 옵션).
    pub default_limit_bytes: u64,
    pub default_min_free_bytes: u64,
    /// 데이터 경로 디스크의 여유 공간. 알 수 없으면 `null`.
    #[serde(default)]
    pub disk_free_bytes: Option<u64>,
    /// 이 데몬이 켜진 뒤 정리로 비운 블롭 수와 바이트.
    #[serde(default)]
    pub evicted_count: u64,
    #[serde(default)]
    pub evicted_bytes: u64,
}

/// `blob.cache_clear` 결과.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BlobCacheClearResult {
    pub evicted_count: u32,
    pub freed_bytes: u64,
}

/// `blob.pins`의 한 줄.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobPinView {
    pub space: SpaceId,
    pub blob: BlobId,
    pub pinned_at: Timestamp,
    /// 원본(평문) 크기. 모르면 0.
    #[serde(default)]
    pub size: u64,
    /// 다 받았는지(고정하면 받기 시작합니다).
    #[serde(default)]
    pub downloaded: bool,
}

/// `library.list`의 한 줄: 첨부를 실은 메시지 하나.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LibraryItem {
    /// `space`·`channel`을 채웁니다. 전체 목록에서는 첨부 썸네일이 빠집니다.
    pub message: MessageView,
    /// 이 기기가 그 메시지를 받은 시각(이 기기에서 쓴 글이면 만든 시각).
    pub received_at: Timestamp,
    /// 그 메시지와 첨부를 남에게 내주지 않음(`seed.exclude_message`).
    pub excluded: bool,
}

/// `library.list` 한 쪽.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LibraryPage {
    pub items: Vec<LibraryItem>,
    /// 쪽을 나누는 전체 후보 수. `offset`이 이 값에 닿을 때까지 읽으면 됩니다(지운 글 등은 쪽에서 빠져 쪽이 짧을 수 있음).
    pub total: u32,
}

// ───────────── 전송 속도·범위별 제한 ─────────────

/// `transfer.rates`(메서드·알림): 이 기기의 지금 전송 속도. 요청·응답(메시지 동기화·첨부 조각)으로 주고받는 바이트를 모든
/// 연결(Tor, 직접 연결, 파일 회로)에서 셉니다. 통화 미디어는 앱이 따로 주고받아 들어가지 않습니다.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TransferRates {
    /// 올리기(바이트/초).
    pub up_bps: u64,
    /// 받기(바이트/초).
    pub down_bps: u64,
    /// 이 기기가 내주는 첨부를 지금 받고 있는 사람(첨부·사람마다). 자동화 토큰에는 비웁니다.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uploads: Vec<UploadRate>,
}

/// 내가 내주는 첨부 하나를 받는 사람 한 명.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadRate {
    pub space: SpaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<ChannelId>,
    pub blob: BlobId,
    /// 받는 사람. 익명 채널의 첨부면 [`ANONYMOUS_USER`].
    pub user: UserId,
    /// 그 사람에게 보내는 속도(바이트/초, 최근 몇 초 평균).
    pub bps: u64,
}

/// 속도 제한을 거는 범위.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RateScope {
    /// 이 크루(1대1 스페이스 포함)와 주고받는 것 전부.
    Crew { space: SpaceId },
    /// 이 채널의 메시지·첨부.
    Channel { space: SpaceId, channel: ChannelId },
    /// 이 사람과 주고받는 것 전부(모든 크루).
    User { user: UserId },
}

/// `rate.limits`의 한 줄. 값은 바이트/초, `null`이면 그 방향은 이 범위에서 제한하지 않음.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitView {
    pub scope: RateScope,
    #[serde(default)]
    pub up_bps: Option<u64>,
    #[serde(default)]
    pub down_bps: Option<u64>,
}

/// `crew.committer_candidates`의 한 줄: 확정 기기가 될 수 있는 기기(소유자·관리자의 활성 기기).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitterCandidate {
    pub user: UserId,
    pub device: DeviceId,
    /// 기기 목록에 적힌 이름.
    pub name: String,
    /// 지금 연결돼 인증된 기기인지(이 기기면 true).
    #[serde(default)]
    pub online: bool,
    /// 지금 확정 기기인지.
    #[serde(default)]
    pub current: bool,
    /// 이 데몬의 기기인지.
    #[serde(default)]
    pub mine: bool,
}

// ───────────── 모더레이션 ─────────────

/// `member.bans`의 한 줄.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BanView {
    pub user: UserId,
    /// 받은 적 있는 프로필. 모르는 사용자면 `null`.
    #[serde(default)]
    pub profile: Option<Profile>,
    #[serde(default)]
    pub reason: Option<String>,
    pub banned_by: UserId,
    pub banned_at: Timestamp,
}

/// `user.blocked`의 한 줄.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedView {
    pub user: UserId,
    #[serde(default)]
    pub profile: Option<Profile>,
    pub blocked_at: Timestamp,
}

/// 남의 기기 하나(`user.devices`): 이 기기가 아는 그 사용자의 서명 기기 목록의 항목과 이 기기에서 차단했는지.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserDeviceView {
    pub device: DeviceId,
    pub name: String,
    pub added_at: Timestamp,
    pub blocked: bool,
}

/// 내 기기 하나(`device.list`): 서명 목록의 항목과 휴대폰인지(내 기기끼리 알려 준 값, 모르면 거짓).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceView {
    #[serde(flatten)]
    pub entry: DeviceEntry,
    pub mobile: bool,
}

/// 이 기기에서 차단한 남의 기기(`user.blocked_devices`). 이름은 그 사용자의 기기 목록에서(모르면 빈 문자열).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedDeviceView {
    pub user: UserId,
    pub device: DeviceId,
    pub name: String,
    #[serde(default)]
    pub profile: Option<Profile>,
    pub blocked_at: Timestamp,
}

/// `message.report`의 결과.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportRef {
    pub report_id: String,
}

/// 신고 처리 상태.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportStatus {
    Open,
    Resolved,
}

/// 신고 처리 방법. 대상은 신고된 메시지(와 그 작성자)입니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportAction {
    /// 아무것도 하지 않고 닫기.
    Dismiss,
    DeleteMessage,
    Kick,
    Ban,
}

impl ReportAction {
    pub fn as_str(self) -> &'static str {
        match self {
            ReportAction::Dismiss => "dismiss",
            ReportAction::DeleteMessage => "delete_message",
            ReportAction::Kick => "kick",
            ReportAction::Ban => "ban",
        }
    }
}

impl FromStr for ReportAction {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "dismiss" => Ok(ReportAction::Dismiss),
            "delete_message" => Ok(ReportAction::DeleteMessage),
            "kick" => Ok(ReportAction::Kick),
            "ban" => Ok(ReportAction::Ban),
            other => Err(format!(
                "알 수 없는 처리 방법: {other} (dismiss, delete_message, kick, ban 중 하나)"
            )),
        }
    }
}

/// 운영진(추방·밴·메시지 관리 권한이 있는 멤버)에게만 보이는 신고.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportView {
    pub report_id: String,
    pub space: SpaceId,
    pub channel: ChannelId,
    pub message: EventId,
    pub reporter: UserId,
    pub reason: String,
    pub reported_at: Timestamp,
    pub status: ReportStatus,
    /// 처리 결과(운영진끼리만 공유). 열린 신고면 `null`.
    #[serde(default)]
    pub resolution: Option<String>,
}

// ───────────── 보관 전용 노드 ─────────────

/// `archive.list`의 한 줄(멤버 쪽).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveView {
    pub device: DeviceId,
    pub user: UserId,
    pub name: String,
    /// 함께 보관하는 비공개 채널(공개 채널은 늘 보관).
    #[serde(default)]
    pub private_channels: Vec<ChannelId>,
    pub granted_by: UserId,
    pub granted_at: Timestamp,
    /// 지금 인증된 연결이 있는지.
    #[serde(default)]
    pub online: bool,
}

/// `archive.hosted`의 한 줄(보관 노드 쪽). 내용 없이 수치만.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveHostedView {
    pub space: SpaceId,
    pub owner: UserId,
    pub members: u32,
    pub channels: u32,
    /// 받아 둔 이벤트 수(헤더만 남은 것 포함).
    pub events: u64,
    /// 받아 둔 첨부 수.
    pub blobs: u32,
    /// 용량 상한에 세는 바이트(모든 이벤트의 헤더·본문과 첨부).
    pub used_bytes: u64,
    pub designated_by: UserId,
    pub designated_at: Timestamp,
    /// 마지막으로 받은 명단의 발행 시각.
    pub roster_issued_at: Timestamp,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use yangpa_core::model::ChannelKind;

    fn sample_message() -> MessageView {
        MessageView {
            id: EventId([1; 32]),
            author: UserId([2; 32]),
            author_device: DeviceId([3; 32]),
            created_at: Timestamp(1_700_000_000_000),
            lamport: 9,
            text: "안녕".into(),
            attachments: vec![],
            embeds: vec![],
            reply_to: None,
            reactions: vec![ReactionView {
                emoji: "<:yangpa:123>".into(),
                count: 2,
                me: true,
            }],
            edited_at: None,
            deleted: false,
            volatile: false,
            expires_at: None,
            thread: None,
            space: None,
            channel: None,
            author_blocked: false,
            anonymous: false,
            anon_name: None,
            mine: false,
            mentions_me: false,
        }
    }

    #[test]
    fn message_view_shape() {
        let v = serde_json::to_value(sample_message()).unwrap();
        let obj = v.as_object().unwrap();
        for key in [
            "id",
            "author",
            "author_device",
            "created_at",
            "lamport",
            "text",
            "attachments",
            "embeds",
            "reactions",
            "deleted",
            "volatile",
            "author_blocked",
        ] {
            assert!(obj.contains_key(key), "{key} 없음");
        }
        // 선택 필드는 없으면 빠집니다.
        assert!(!obj.contains_key("reply_to"));
        assert!(!obj.contains_key("expires_at"));
        assert_eq!(v["created_at"], json!(1_700_000_000_000u64));
        assert_eq!(v["id"], json!(EventId([1; 32]).to_string()));
        assert_eq!(
            v["reactions"][0],
            json!({"emoji": "<:yangpa:123>", "count": 2, "me": true})
        );
        let back: MessageView = serde_json::from_value(v).unwrap();
        assert_eq!(back, sample_message());
    }

    #[test]
    fn redaction_strips_content() {
        let mut m = sample_message();
        m.volatile = true;
        let r = m.redacted();
        assert!(r.text.is_empty() && r.reactions.is_empty());
        assert_eq!(r.id, m.id);
    }

    #[test]
    fn space_summary_and_channel_view_shape() {
        let s = SpaceSummary {
            space: SpaceId([5; 16]),
            kind: SpaceKind::Crew,
            name: "게임 모임".into(),
            icon: None,
            unread: 3,
            mention: 1,
            dormant: false,
            member_count: 4,
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["kind"], "crew");
        assert_eq!(v["member_count"], 4);
        assert!(v.get("icon").is_none());

        let mut settings = ChannelSettings::text("공지");
        settings.kind = ChannelKind::Forum;
        let c = ChannelView {
            channel: ChannelId([6; 16]),
            settings,
            unread: 0,
            joined: None,
            participants: None,
            api_read_allowed: false,
            p2p: true,
            p2p_joined: false,
            anonymous: false,
            featured_post: None,
            attach_limit: None,
            no_edit: false,
            no_delete: false,
            volatile_cap: None,
            volatile_full: false,
            has_new: false,
        };
        let mut v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["settings"]["kind"], "forum");
        assert_eq!(v["settings"]["name"], "공지");
        assert_eq!(v["p2p"], true);
        assert_eq!(v["p2p_joined"], false);
        assert_eq!(serde_json::from_value::<ChannelView>(v.clone()).unwrap(), c);
        // P2P 필드가 없는 옛 응답은 Tor 채널로 읽힙니다.
        let obj = v.as_object_mut().unwrap();
        obj.remove("p2p");
        obj.remove("p2p_joined");
        let old = serde_json::from_value::<ChannelView>(v).unwrap();
        assert!(!old.p2p && !old.p2p_joined);
    }

    #[test]
    fn policy_dto_shape_and_defaults() {
        let p = SeederPolicy {
            enabled: true,
            types: vec![DataType::Text, DataType::HistoryBundle],
            include_channels: None,
            all_channels: false,
            exclude_channels: vec![],
            excluded_events: vec![],
            upload_limit_bps: Some(2 * 1024 * 1024),
            max_concurrent: 4,
            prefetch: false,
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["types"], json!(["text", "history_bundle"]));
        assert_eq!(v["include_channels"], Value::Null);
        // 예전 데몬의 답(prefetch 없음)도 읽힘.
        let mut old = v.clone();
        old.as_object_mut().unwrap().remove("prefetch");
        assert_eq!(serde_json::from_value::<SeederPolicy>(old).unwrap(), p);
        let mut pri = BTreeMap::new();
        pri.insert(DataType::Image, 200u8);
        let peer = PeerPolicy {
            types: DataType::ALL.to_vec(),
            priorities: pri,
            download_limit_bps: None,
            auto_download_max_bytes: 20 << 20,
        };
        let v = serde_json::to_value(&peer).unwrap();
        assert_eq!(v["priorities"], json!({"image": 200}));
        assert_eq!(serde_json::from_value::<PeerPolicy>(v).unwrap(), peer);
        assert_eq!(
            "history_bundle".parse::<DataType>().unwrap(),
            DataType::HistoryBundle
        );
    }

    #[test]
    fn net_status_shape() {
        let n = NetStatus {
            peer_id: "12D3KooW".into(),
            mode: NetMode::Tor,
            nat: NatStatus::Disabled,
            bootstrap_progress: Some(40),
            ..Default::default()
        };
        let v = serde_json::to_value(&n).unwrap();
        for key in [
            "peer_id",
            "mode",
            "listen_addrs",
            "relay_addrs",
            "nat",
            "connections",
            "bootstrap_progress",
        ] {
            assert!(v.get(key).is_some(), "{key} 없음");
        }
        assert_eq!(v["mode"], "tor");
        assert_eq!(v["nat"], "disabled");
        // 최소 필드만 있는 응답도 읽힙니다.
        let min: NetStatus =
            serde_json::from_value(json!({"peer_id": "x", "mode": "normal"})).unwrap();
        assert_eq!(min.connections, 0);
    }

    #[test]
    fn moderation_shapes() {
        let b = BanView {
            user: UserId([1; 32]),
            profile: None,
            reason: None,
            banned_by: UserId([2; 32]),
            banned_at: Timestamp(7),
        };
        let v = serde_json::to_value(&b).unwrap();
        assert_eq!(v["profile"], Value::Null);
        assert_eq!(v["reason"], Value::Null);
        assert_eq!(v["banned_at"], json!(7));
        let r = ReportView {
            report_id: "abc".into(),
            space: SpaceId([1; 16]),
            channel: ChannelId([2; 16]),
            message: EventId([3; 32]),
            reporter: UserId([4; 32]),
            reason: "스팸".into(),
            reported_at: Timestamp(9),
            status: ReportStatus::Open,
            resolution: None,
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["status"], "open");
        assert_eq!(v["resolution"], Value::Null);
        assert_eq!(serde_json::from_value::<ReportView>(v).unwrap(), r);
        for a in [
            ReportAction::Dismiss,
            ReportAction::DeleteMessage,
            ReportAction::Kick,
            ReportAction::Ban,
        ] {
            assert_eq!(a.as_str().parse::<ReportAction>().unwrap(), a);
            assert_eq!(serde_json::to_value(a).unwrap(), json!(a.as_str()));
        }
        assert!("nuke".parse::<ReportAction>().is_err());
    }
}
