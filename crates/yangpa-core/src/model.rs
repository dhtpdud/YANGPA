//! 공간·채널·역할·권한 모델.
//!
//! 스페이스(1대1)와 크루는 같은 구조이고 [`SpaceKind`]만 다릅니다.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::ids::{ChannelId, DeviceId, EmojiId, EventId, KeyGroupId, RoleId, SpaceId, UserId};
use crate::time::Timestamp;

/// 공간 종류. 1대1 → 크루 전환은 이 값 하나만 바뀝니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpaceKind {
    /// 두 친구 사이의 1대1 공간(멤버 2명짜리 크루).
    Space,
    /// 디스코드 서버에 해당하는 커뮤니티.
    Crew,
    /// 본인 기기만 멤버인 공간. 친구 메모·설정 동기화용.
    SelfSpace,
}

/// 채널 종류. 이후 종류가 추가돼도 옛 노드가 깨지지 않게 `Other`를 둡니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Text,
    Voice,
    Forum,
    /// 카테고리(채널 트리의 묶음). 메시지를 담지 않음.
    Category,
    Other(u16),
}

/// 보안 등급.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SecurityLevel {
    /// 양측 디스크(SQLCipher)에 저장.
    #[default]
    Normal,
    /// RAM 밖으로 나가지 않음.
    Volatile,
}

/// 채널 내용(메시지·키 레인, 첨부·통화)이 오가는 길.
///
/// 상태 레인(채널이 있다는 것과 이 표시)은 늘 Tor로 동기화합니다. 채널 관리자가 양쪽으로 바꿀 수 있습니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChannelTransport {
    #[default]
    Tor,
    /// 참여자끼리 직접 연결(서로 IP가 보임). 이 기기가 경고를 받아들인 뒤에만 참여합니다.
    P2p,
}

/// 참여 방식.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum JoinMode {
    /// 채널을 볼 수 있는 멤버는 항상 참여.
    #[default]
    Always,
    /// 채널에 들어가 있는 동안만 대화 참여.
    JoinLeave,
}

/// 채팅 수명.
///
/// JSON에서는 `{"mode": "seconds", "seconds": 30}`, 이벤트 본문(postcard)에서는 바깥 태그 표현입니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(
    remote = "Self",
    rename_all = "snake_case",
    tag = "mode",
    content = "seconds"
)]
pub enum ChatTtl {
    #[default]
    None,
    /// 퇴장 시 그 사용자 기기에서 삭제.
    OnLeave,
    /// 수신 시점 기준 N초 뒤 만료.
    Seconds(u32),
}

/// [`ChatTtl`]의 이진 형식 표현.
#[allow(dead_code)]
#[derive(Serialize, Deserialize)]
#[serde(remote = "ChatTtl")]
enum ChatTtlBin {
    None,
    OnLeave,
    Seconds(u32),
}

tagged_only_for_humans!(ChatTtl, ChatTtlBin);

/// 과거 기록 공개(새 입장자가 입장 이전 기록을 볼 수 있는지).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HistoryVisibility {
    Public,
    #[default]
    Private,
}

/// 채널 설정. 채널 생성·변경 상태 이벤트의 본문.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelSettings {
    pub name: String,
    pub kind: ChannelKind,
    #[serde(default)]
    pub topic: String,
    #[serde(default)]
    pub parent: Option<ChannelId>,
    #[serde(default)]
    pub position: i32,
    #[serde(default)]
    pub security: SecurityLevel,
    #[serde(default)]
    pub join_mode: JoinMode,
    #[serde(default)]
    pub ttl: ChatTtl,
    #[serde(default)]
    pub history: HistoryVisibility,
    /// 링크 미리보기 생성 허용. 휘발성 채널 기본값은 꺼짐.
    #[serde(default = "default_true")]
    pub link_previews: bool,
    /// 역할·멤버별 권한 덮어쓰기.
    #[serde(default)]
    pub overwrites: Vec<PermissionOverwrite>,
    /// 포럼 채널 태그 목록.
    #[serde(default)]
    pub forum_tags: Vec<ForumTag>,
}

fn default_true() -> bool {
    true
}

impl ChannelSettings {
    pub fn text(name: impl Into<String>) -> Self {
        ChannelSettings {
            name: name.into(),
            kind: ChannelKind::Text,
            topic: String::new(),
            parent: None,
            position: 0,
            security: SecurityLevel::Normal,
            join_mode: JoinMode::Always,
            ttl: ChatTtl::None,
            history: HistoryVisibility::Private,
            link_previews: true,
            overwrites: Vec::new(),
            forum_tags: Vec::new(),
        }
    }

    pub fn is_volatile(&self) -> bool {
        self.security == SecurityLevel::Volatile
    }

    /// 키 그룹에서 채널 뿌리를 파생하는 채널인지(휘발성 채널은 RAM 전용 세션 키, 카테고리는 내용이 없음). 어느
    /// 그룹을 쓰는지는 [`SpaceState::channel_key_group`](`None`이면 크루 그룹).
    pub fn uses_group_keys(&self) -> bool {
        !self.is_volatile() && self.kind != ChannelKind::Category
    }

    /// 특정 역할만 볼 수 있는 비공개 채널인지(@everyone에게 VIEW_CHANNEL이 거부됨).
    pub fn is_private(&self) -> bool {
        self.overwrites.iter().any(|o| {
            o.target == OverwriteTarget::Role(RoleId::EVERYONE)
                && o.deny.contains(Permissions::VIEW_CHANNEL)
        })
    }

    /// 보기 권한(VIEW_CHANNEL)을 정하는 덮어쓰기만 정규화한 바이트열. 이것이 같은 채널들은 어떤 상태에서도 볼 수
    /// 있는 사람이 같습니다([`SpaceState::channel_permissions`]는 VIEW_CHANNEL을 기본 권한과 이 비트들로만 정함).
    /// @everyone·멤버 덮어쓰기는 처음 것만, 역할 덮어쓰기는 모두 합쳐 적용하는 규칙을 그대로 따릅니다.
    pub fn view_rule(&self) -> Vec<u8> {
        let view = Permissions::VIEW_CHANNEL;
        let mut entries: BTreeMap<(u8, Vec<u8>), (bool, bool)> = BTreeMap::new();
        for o in &self.overwrites {
            let (tag, id, first_only) = match o.target {
                OverwriteTarget::Role(r) => (0u8, r.as_bytes().to_vec(), r == RoleId::EVERYONE),
                OverwriteTarget::Member(u) => (1u8, u.as_bytes().to_vec(), true),
            };
            let (allow, deny) = (o.allow.contains(view), o.deny.contains(view));
            let slot = entries.entry((tag, id));
            match slot {
                std::collections::btree_map::Entry::Vacant(e) => {
                    e.insert((allow, deny));
                }
                std::collections::btree_map::Entry::Occupied(mut e) if !first_only => {
                    let v = e.get_mut();
                    v.0 |= allow;
                    v.1 |= deny;
                }
                std::collections::btree_map::Entry::Occupied(_) => {}
            }
        }
        let mut out = Vec::new();
        for ((tag, id), (allow, deny)) in entries {
            if allow || deny {
                out.push(tag);
                out.extend_from_slice(&id);
                out.push(u8::from(allow) | (u8::from(deny) << 1));
            }
        }
        out
    }
}

impl SpaceState {
    /// 채널 `ch`의 비공개 키 그룹([`KeyGroupId::private`], 열람 규칙에서 나옴)과 그 `KeyGroup` 스트림. 보기 규칙
    /// ([`ChannelSettings::view_rule`])이 비었고 @everyone이 볼 수 있는 채널은 모든 멤버가 보므로 크루 그룹
    /// ([`KeyGroupId::crew`], 갱신은 공간 스트림)을 쓰고 비공개 그룹이 없습니다: 같은 사람들에게 ID가 둘이면 한쪽 갱신을
    /// 따라잡지 않는 멤버가 생깁니다. 휘발성 채널(키를 RAM 전용 스트림에서만 나눔)과 카테고리(내용 없음)도 없습니다.
    /// 상태만 보는 구조 규칙이라 모든 노드가 같은 답을 냅니다(규칙이 비지 않았는데 마침 모두가 보는 채널은 비공개
    /// 그룹을 그대로 씀).
    pub fn channel_key_group(&self, space: &SpaceId, ch: &ChannelSettings) -> Option<KeyGroupId> {
        self.channel_key_group_with(space, ch, self.everyone_views())
    }

    fn channel_key_group_with(
        &self,
        space: &SpaceId,
        ch: &ChannelSettings,
        everyone_views: bool,
    ) -> Option<KeyGroupId> {
        if ch.is_volatile() || ch.kind == ChannelKind::Category {
            return None;
        }
        let rule = ch.view_rule();
        if rule.is_empty() && everyone_views {
            return None;
        }
        Some(KeyGroupId::private(space, &rule))
    }

    /// @everyone 역할만으로 채널을 볼 수 있는지(보기 덮어쓰기가 없는 채널을 모든 멤버가 봄).
    fn everyone_views(&self) -> bool {
        self.roles.get(&RoleId::EVERYONE).is_some_and(|r| {
            r.permissions.contains(Permissions::VIEW_CHANNEL)
                || r.permissions.contains(Permissions::ADMINISTRATOR)
        })
    }

    /// 키 그룹 `group`을 쓰는 채널이 지금 상태에 있는지.
    pub fn has_key_group(&self, space: &SpaceId, group: &KeyGroupId) -> bool {
        let everyone = self.everyone_views();
        self.channels
            .values()
            .any(|ch| self.channel_key_group_with(space, ch, everyone).as_ref() == Some(group))
    }

    /// 이 사용자가 키 그룹 `group`의 스트림을 받을 자격이 있는지: 멤버이고 그 그룹의 채널 하나라도 볼 수 있으면.
    pub fn can_view_key_group(&self, space: &SpaceId, group: &KeyGroupId, user: &UserId) -> bool {
        let everyone = self.everyone_views();
        self.is_member(user)
            && self.channels.iter().any(|(c, ch)| {
                self.channel_key_group_with(space, ch, everyone).as_ref() == Some(group)
                    && self
                        .channel_permissions(user, c)
                        .contains(Permissions::VIEW_CHANNEL)
            })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForumTag {
    pub id: u32,
    pub name: String,
    #[serde(default)]
    pub emoji: Option<String>,
}

/// 권한 비트. 디스코드와 비슷한 구성.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Permissions(pub u64);

impl Permissions {
    pub const NONE: Permissions = Permissions(0);
    pub const VIEW_CHANNEL: Permissions = Permissions(1 << 0);
    pub const SEND_MESSAGES: Permissions = Permissions(1 << 1);
    pub const READ_HISTORY: Permissions = Permissions(1 << 2);
    pub const ADD_REACTIONS: Permissions = Permissions(1 << 3);
    pub const ATTACH_FILES: Permissions = Permissions(1 << 4);
    pub const EMBED_LINKS: Permissions = Permissions(1 << 5);
    pub const USE_EXTERNAL_EMOJI: Permissions = Permissions(1 << 6);
    pub const CONNECT_VOICE: Permissions = Permissions(1 << 7);
    pub const SPEAK: Permissions = Permissions(1 << 8);
    pub const STREAM: Permissions = Permissions(1 << 9);
    pub const CREATE_POSTS: Permissions = Permissions(1 << 10);
    /// 크루가 정한 STUN 서버를 이 크루의 P2P 연결·통화에 씀(멤버가 따로 켜야 씀).
    pub const USE_CREW_STUN: Permissions = Permissions(1 << 11);
    // 1 << 12: 예전 `use_crew_turn`(2026-11-07 TURN 삭제). 옛 역할에 켜져 있을 수 있어 다른 뜻으로 다시 쓰지 않습니다.
    /// 한 메시지에 20명이 넘게 멘션해도 알림 토픽에 실림(넘으면 중계 노드가 알림 토픽에서 버림).
    pub const MENTION_MANY: Permissions = Permissions(1 << 13);
    pub const MANAGE_MESSAGES: Permissions = Permissions(1 << 16);
    pub const MANAGE_CHANNELS: Permissions = Permissions(1 << 17);
    pub const MANAGE_ROLES: Permissions = Permissions(1 << 18);
    pub const KICK_MEMBERS: Permissions = Permissions(1 << 19);
    pub const CREATE_INVITE: Permissions = Permissions(1 << 20);
    pub const MANAGE_EMOJI: Permissions = Permissions(1 << 21);
    pub const MANAGE_VOICE: Permissions = Permissions(1 << 22);
    pub const MANAGE_SPACE: Permissions = Permissions(1 << 23);
    /// 크루에서 내보내고 다시 못 들어오게 막기(밴).
    pub const BAN_MEMBERS: Permissions = Permissions(1 << 24);
    pub const ADMINISTRATOR: Permissions = Permissions(1 << 31);

    /// 새 크루의 @everyone 기본 권한.
    pub const DEFAULT_EVERYONE: Permissions = Permissions(
        Self::VIEW_CHANNEL.0
            | Self::SEND_MESSAGES.0
            | Self::READ_HISTORY.0
            | Self::ADD_REACTIONS.0
            | Self::ATTACH_FILES.0
            | Self::EMBED_LINKS.0
            | Self::CONNECT_VOICE.0
            | Self::SPEAK.0
            | Self::STREAM.0
            | Self::CREATE_POSTS.0,
    );

    /// 모든 권한.
    pub const ALL: Permissions = Permissions(u64::MAX);

    /// 이름 ↔ 비트 대응(CLI·API에서 `manage_channels,kick` 형태로 씀).
    pub const NAMES: &'static [(&'static str, Permissions)] = &[
        ("view_channel", Self::VIEW_CHANNEL),
        ("send_messages", Self::SEND_MESSAGES),
        ("read_history", Self::READ_HISTORY),
        ("add_reactions", Self::ADD_REACTIONS),
        ("attach_files", Self::ATTACH_FILES),
        ("embed_links", Self::EMBED_LINKS),
        ("use_external_emoji", Self::USE_EXTERNAL_EMOJI),
        ("connect_voice", Self::CONNECT_VOICE),
        ("speak", Self::SPEAK),
        ("stream", Self::STREAM),
        ("create_posts", Self::CREATE_POSTS),
        ("use_crew_stun", Self::USE_CREW_STUN),
        ("mention_many", Self::MENTION_MANY),
        ("manage_messages", Self::MANAGE_MESSAGES),
        ("manage_channels", Self::MANAGE_CHANNELS),
        ("manage_roles", Self::MANAGE_ROLES),
        ("kick", Self::KICK_MEMBERS),
        ("create_invite", Self::CREATE_INVITE),
        ("manage_emoji", Self::MANAGE_EMOJI),
        ("manage_voice", Self::MANAGE_VOICE),
        ("manage_space", Self::MANAGE_SPACE),
        ("ban", Self::BAN_MEMBERS),
        ("administrator", Self::ADMINISTRATOR),
    ];

    pub const fn contains(self, other: Permissions) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Permissions) -> Permissions {
        Permissions(self.0 | other.0)
    }

    pub const fn remove(self, other: Permissions) -> Permissions {
        Permissions(self.0 & !other.0)
    }

    pub fn parse_list(s: &str) -> Result<Permissions, String> {
        let mut p = Permissions::NONE;
        for name in s.split(',').map(str::trim).filter(|n| !n.is_empty()) {
            let (_, bit) = Self::NAMES
                .iter()
                .find(|(n, _)| *n == name)
                .ok_or_else(|| format!("알 수 없는 권한: {name}"))?;
            p = p.union(*bit);
        }
        Ok(p)
    }

    pub fn names(self) -> Vec<&'static str> {
        Self::NAMES
            .iter()
            .filter(|(_, b)| self.contains(*b))
            .map(|(n, _)| *n)
            .collect()
    }
}

impl std::ops::BitOr for Permissions {
    type Output = Permissions;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

impl std::fmt::Debug for Permissions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Permissions({:?})", self.names())
    }
}

/// JSON에서는 `{"type": "role", "id": ...}`, 이벤트 본문(postcard)에서는 바깥 태그 표현입니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(
    remote = "Self",
    rename_all = "snake_case",
    tag = "type",
    content = "id"
)]
pub enum OverwriteTarget {
    Role(RoleId),
    Member(UserId),
}

/// [`OverwriteTarget`]의 이진 형식 표현.
#[allow(dead_code)]
#[derive(Serialize, Deserialize)]
#[serde(remote = "OverwriteTarget")]
enum OverwriteTargetBin {
    Role(RoleId),
    Member(UserId),
}

tagged_only_for_humans!(OverwriteTarget, OverwriteTargetBin);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PermissionOverwrite {
    pub target: OverwriteTarget,
    #[serde(default)]
    pub allow: Permissions,
    #[serde(default)]
    pub deny: Permissions,
}

/// 역할.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Role {
    pub id: RoleId,
    pub name: String,
    /// 표시 색(0xRRGGBB).
    #[serde(default)]
    pub color: u32,
    /// 순위. 클수록 높음. @everyone은 0.
    pub rank: u32,
    pub permissions: Permissions,
    /// 자동화(AI) 역할 표시.
    #[serde(default)]
    pub automation: bool,
}

impl Role {
    pub fn everyone(perms: Permissions) -> Self {
        Role {
            id: RoleId::EVERYONE,
            name: "@everyone".into(),
            color: 0,
            rank: 0,
            permissions: perms,
            automation: false,
        }
    }

    /// 위아래를 가릴 때 쓰는 순위. 관리자 권한 역할은 `rank`와 상관없이 모든 일반 역할보다 위에 있습니다(관리자 역할이
    /// 맨 아래 순위로 만들어져도 관리자가 일반 역할을 만들고 고칠 수 있게). 관리자 역할끼리, 일반 역할끼리는 `rank` 순.
    pub fn level(&self) -> u64 {
        let admin = if self.permissions.contains(Permissions::ADMINISTRATOR) {
            ADMIN_LEVEL
        } else {
            0
        };
        admin + u64::from(self.rank)
    }
}

/// 관리자 권한 역할의 순위에 더하는 값(`Role::level`). 어떤 `rank`(u32)보다도 큽니다.
pub const ADMIN_LEVEL: u64 = 1 << 32;

/// 역할 표시 설정. 화면(로컬 API 보기)에서만 거릅니다: 멤버 기기는 작성자 권한을 직접 확인해야 해서 누가 어떤
/// 역할인지는 공간 상태에 그대로 남습니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleDisplay {
    /// 이 역할을 볼 수 있는 역할. `None`이면 모든 멤버. 역할을 가진 사람과 역할 관리자는 늘 봅니다.
    #[serde(default)]
    pub visible_to: Option<Vec<RoleId>>,
    /// 이 역할을 가진 사람이 관리자(소유자·관리자 권한 역할)가 누구인지 보는지. 권한처럼 더해집니다: 멤버의
    /// 역할(@everyone 포함) 중 하나라도 켜져 있으면 봅니다. 기본값은 @everyone만 켬(지금처럼 모두가 봄).
    #[serde(default)]
    pub see_admins: bool,
}

impl RoleDisplay {
    /// 설정하지 않은 역할의 값.
    pub fn default_for(role: &RoleId) -> Self {
        RoleDisplay {
            visible_to: None,
            see_admins: *role == RoleId::EVERYONE,
        }
    }
}

/// 공간 멤버(사용자 단위). 기기는 서명된 [`crate::device::DeviceList`]를 간추린 기록으로 따로 둡니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub user: UserId,
    #[serde(default)]
    pub roles: Vec<RoleId>,
    /// 크루 프로필(크루별 닉네임 등). 없으면 기본 프로필.
    #[serde(default)]
    pub nickname: Option<String>,
    /// 자동화(AI) 클라이언트가 이 멤버로 동작 중인지.
    #[serde(default)]
    pub automation: bool,
}

/// 공간 전체 설정.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceSettings {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub icon: Option<crate::ids::BlobId>,
    /// 아이콘 블롭의 복호화 키. 상태 레인(상태 키로 암호화)에만 실려 멤버만 엽니다. 이모지 키처럼 공간 상태에 두되,
    /// 사람이 읽는 형식(로컬 API JSON)에서는 늘 비워 내보냅니다.
    #[serde(default, with = "crate::device::binary_only")]
    pub icon_key: Option<crate::body::Secret32>,
    /// 다른 공간의 커스텀 이모지 사용 허용(미결정 사항: 기본 꺼짐).
    #[serde(default)]
    pub allow_external_emoji: bool,
    /// 엄격한 키 회전: 내보낸 멤버를 뺀 키 갱신이 올 때까지 메시지를 끝없이 기다립니다(끄면 `rotation_wait` 뒤 옛 키로
    /// 보내고 `key_rotation_pending`을 켬). 기본 꺼짐.
    #[serde(default)]
    pub strict_rotation: bool,
}

/// 큰 표. 복제가 O(1)이고 고칠 때 바뀐 경로의 마디만 복사하는 지속 B-트리라, 큰 크루(3만 명)에서 해소기 스냅숏·데몬
/// 사본이 표 크기에 비례하지 않습니다. 순회 순서와 직렬화(맵)는 `BTreeMap`과 같습니다.
pub type SharedMap<K, V> = imbl::OrdMap<K, V>;

/// [`SharedMap`] 두 판의 차이 항목(`diff`: 나눠 쓰는 마디는 건너뜀). 저장(바뀐 행만 씀)·색인(바뀐 멤버만 고침)이 씁니다.
pub use imbl::ordmap::DiffItem;

/// 해소된 공간 상태(상태 이벤트를 결정론적 순서로 적용한 결과).
///
/// 멤버 수만큼 커질 수 있는 표(멤버·기기 목록·밴)와 한도까지 채우면 메가바이트 단위가 되는 관리 설정(채널: 덮어쓰기·
/// 주제·포럼 태그, 초대: 역할 목록, 역할 표시: 볼 수 있는 역할 목록)은 지속 자료 구조([`SharedMap`], `imbl::Vector`)
/// 이고, 나머지는 작아(한도까지 채워도 합쳐 수백 KB) `BTreeMap`입니다. 멤버와 기기 기록 값은 `Arc`로 듭니다: 나눠
/// 쓰던 마디를 고칠 때 마디 하나(16항목)를 복사하므로 값째 들면 그만큼 복사합니다(바꿀 때는 `Arc::make_mut`).
/// 기기는 키 본문·이름 없이 간추린 기록([`crate::device::MemberDevices`])만 둡니다: 키는 키 약속으로 찾는 표에
/// 있어 3만 명 상태가 상자 키(1.2KB)·FN-DSA 공개키(0.9KB)를 기기마다 품지 않습니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceState {
    pub kind: SpaceKind,
    pub owner: UserId,
    /// 키 그룹 갱신(`KeyUpdate`)을 내는 기기(확정 기기).
    pub committer: DeviceId,
    pub settings: SpaceSettings,
    pub members: SharedMap<UserId, Arc<Member>>,
    pub roles: BTreeMap<RoleId, Role>,
    pub channels: SharedMap<ChannelId, ChannelSettings>,
    /// 멤버별 최신 기기 목록(서명 검증을 거친 것만)의 간추린 기록. 이벤트 작성 기기가 여기 있어야 유효합니다.
    #[serde(default)]
    pub devices: SharedMap<UserId, Arc<crate::device::MemberDevices>>,
    /// 유효한 초대(폐기·만료되지 않은 것). 사용 횟수는 적용된 `MemberJoin` 수로 셉니다.
    #[serde(default)]
    pub invites: imbl::Vector<InviteState>,
    /// 커스텀 이모지.
    #[serde(default)]
    pub emojis: BTreeMap<EmojiId, crate::body::CustomEmoji>,
    /// 크루 전환을 요청한 사용자(상대 동의 대기 중).
    #[serde(default)]
    pub pending_crew_conversion: Option<UserId>,
    /// 차단(밴)된 사용자. 여기 있는 사용자의 `MemberJoin`은 어느 노드에서나 거부됩니다.
    #[serde(default)]
    pub bans: SharedMap<UserId, BanInfo>,
    /// 지정된 보관 전용 노드(기기별). 멤버가 아니며 키를 받지 않습니다.
    #[serde(default)]
    pub archives: BTreeMap<DeviceId, crate::archive::ArchiveEntry>,
    /// 채널별 내용 전송 방식(`Tor`인 채널은 적지 않음). 옛 상태 캐시(JSON)에는 없으므로 기본값이 빈 표입니다.
    #[serde(default)]
    pub channel_transport: BTreeMap<ChannelId, ChannelTransport>,
    /// 역할별 표시 설정(기본값인 역할은 적지 않음). 옛 상태 캐시(JSON)에는 없으므로 기본값이 빈 표입니다.
    #[serde(default)]
    pub role_display: SharedMap<RoleId, RoleDisplay>,
    /// 지금 익명인 채널. 옛 상태 캐시(JSON)에는 없으므로 기본값이 빈 집합입니다.
    #[serde(default)]
    pub anonymous_channels: BTreeSet<ChannelId>,
    /// 크루가 멤버에게 내주는 STUN 서버(`CrewServersSet`). 옛 상태 캐시(JSON)에는 없으므로 기본값이 빈 목록입니다.
    #[serde(default)]
    pub crew_servers: CrewServers,
    /// 포럼 채널마다 대문 게시글(`ForumFeaturedSet`). 옛 상태 캐시(JSON)에는 없으므로 기본값이 빈 표입니다.
    #[serde(default)]
    pub forum_featured: BTreeMap<ChannelId, EventId>,
    /// 채널별 첨부 한도(첨부 하나의 평문 바이트, `ChannelAttachLimitSet`). 없는 채널은 무제한, 0은 첨부 금지. 옛 상태
    /// 캐시(JSON)에는 없으므로 기본값이 빈 표입니다.
    #[serde(default)]
    pub attach_limits: BTreeMap<ChannelId, u64>,
    /// 상시 휘발성 채널마다 참가 기기 상한(`ChannelVolatileCapSet`). 없는 채널은 [`DEFAULT_VOLATILE_CAP`].
    #[serde(default)]
    pub volatile_caps: BTreeMap<ChannelId, u32>,
    /// 채널별 수정·삭제 금지(`ChannelEditRulesSet`). 둘 다 꺼진 채널은 표에 없습니다. 옛 상태 캐시(JSON)에는 없으므로
    /// 기본값이 빈 표입니다.
    #[serde(default)]
    pub edit_rules: BTreeMap<ChannelId, EditRules>,
    /// 크루 커스텀 이모지 한도(`EmojiLimitsSet`). 옛 상태 캐시(JSON)에는 없으므로 기본값(한도 없음)입니다.
    #[serde(default)]
    pub emoji_limits: EmojiLimits,
    /// 커스텀 이모지를 올린 사용자(`EmojiAdd` 작성자). 옛 상태 캐시(JSON)에는 없으므로 기본값이 빈 표입니다.
    #[serde(default)]
    pub emoji_by: BTreeMap<EmojiId, UserId>,
    /// 소유자가 해산함(`CrewDisband`). 그 뒤로는 소유자를 포함해 아무도 권한이 없습니다. 저장된 상태 행·상태 해시가
    /// 그대로이게 직렬화하지 않고, 해소기의 정규 인코딩이 참일 때만 끝에 따로 싣습니다.
    #[serde(skip)]
    pub disbanded: bool,
}

/// 채널의 수정·삭제 금지(`ChannelEditRulesSet`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditRules {
    #[serde(default)]
    pub no_edit: bool,
    #[serde(default)]
    pub no_delete: bool,
}

impl EditRules {
    pub fn is_empty(&self) -> bool {
        !self.no_edit && !self.no_delete
    }
}

/// 크루 커스텀 이모지 한도(`EmojiLimitsSet`). `None`이면 개수는 해소기 상한(500)까지, 크기는 무제한입니다.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmojiLimits {
    /// 이모지 수 상한.
    #[serde(default)]
    pub max_count: Option<u32>,
    /// 이모지 이미지 하나의 크기 상한(바이트).
    #[serde(default)]
    pub max_bytes: Option<u64>,
}

/// 작성자 자신의 후원자 표시(`MemberFlair`, decisions.md "후원자 카드 배너와 후광"). 두 부분은 따로 바뀌고 받는 쪽은
/// 부분마다 `at`이 더 늦은 것을 씁니다. 없는 부분은 그대로 둡니다.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Flair {
    /// 후원 단계. 후원 계정을 연결해 직접 확인한 기기만 싣습니다.
    #[serde(default)]
    pub tier: Option<FlairTier>,
    /// 후광 색. 사용자가 고른 기기가 싣습니다.
    #[serde(default)]
    pub halo: Option<FlairHalo>,
}

impl Flair {
    /// 바꿀 것이 있고 색이 0xRRGGBB 안인지.
    pub fn is_valid(&self) -> bool {
        (self.tier.is_some() || self.halo.is_some())
            && self
                .halo
                .and_then(|h| h.colors)
                .is_none_or(|c| c.iter().all(|&x| x <= 0xFF_FFFF))
    }
}

/// 후원 단계. 금액은 싣지 않고 쓸 수 있는 표시만 싣습니다(크루 멤버에게 후원 금액이 드러나지 않게).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlairTier {
    /// 프로필 카드 배경 배너(월 14달러 이상).
    pub banner: bool,
    /// 프로필 사진 후광(월 50달러 이상).
    pub halo: bool,
    /// 이때까지만 믿습니다.
    pub until: Timestamp,
    /// 확인한 시각.
    pub at: Timestamp,
}

/// 후광 색(0xRRGGBB 두 개, 그라데이션의 양 끝). `None`이면 끔.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlairHalo {
    pub colors: Option<[u32; 2]>,
    /// 고른 시각.
    pub at: Timestamp,
}

/// 크루 STUN 서버 목록. `turn`은 TURN을 없애기(2026-11-07) 전의 칸으로, 옛 이벤트·상태를 그대로 읽으려고 남깁니다.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrewServers {
    /// `host:port`.
    #[serde(default)]
    pub stun: Vec<String>,
    /// 옛 `turn:아이디:비밀번호@host:port` 줄. 새로 쓰지 않고 데몬도 쓰지 않습니다.
    #[serde(default)]
    pub turn: Vec<String>,
}

impl CrewServers {
    pub fn is_empty(&self) -> bool {
        self.stun.is_empty() && self.turn.is_empty()
    }
}

/// 크루 STUN 목록(옛 TURN 목록도)에 둘 수 있는 줄 수.
pub const MAX_CREW_SERVERS: usize = 8;
/// 크루 서버 한 줄의 최대 길이(바이트).
pub const MAX_CREW_SERVER_LEN: usize = 512;
/// 채널 첨부 한도의 최솟값(1MiB). 그보다 작으면 사진 한 장도 못 올립니다. 0(첨부 금지)은 따로 허용합니다.
pub const MIN_ATTACH_LIMIT: u64 = 1024 * 1024;

/// 커스텀 이모지 수 상한(해소기). 크루 한도([`EmojiLimits::max_count`])는 이 안에서만 정합니다.
pub const MAX_EMOJIS: usize = 500;

/// 상시 휘발성 채널 참가 기기 상한 기본값과 정할 수 있는 범위. 회전 한 번이 참가자 수만큼 감싸기(약 1.3KB)를 내므로
/// 위로 묶습니다.
pub const DEFAULT_VOLATILE_CAP: u32 = 200;
pub const MIN_VOLATILE_CAP: u32 = 2;
pub const MAX_VOLATILE_CAP: u32 = 2000;

/// 채널 첨부 한도로 쓸 수 있는 값인지: 0(첨부 금지) 또는 [`MIN_ATTACH_LIMIT`] 이상.
pub fn valid_attach_limit(max_bytes: u64) -> bool {
    max_bytes == 0 || max_bytes >= MIN_ATTACH_LIMIT
}

/// 밴 기록. 상태 레인(상태 키로 암호화)에만 실리므로 크루 멤버만 읽습니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BanInfo {
    #[serde(default)]
    pub reason: String,
    pub banned_by: UserId,
    /// 밴 이벤트의 작성 시각.
    pub banned_at: crate::time::Timestamp,
}

/// 상태에 기록된 초대와 사용 횟수.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteState {
    pub info: crate::body::InviteInfo,
    pub created_by: UserId,
    pub uses: u32,
}

impl InviteState {
    pub fn usable_at(&self, at: crate::time::Timestamp) -> bool {
        self.info.expires_at.is_none_or(|e| at < e)
            && self.info.max_uses.is_none_or(|m| self.uses < m)
    }
}

impl SpaceState {
    /// 사용자의 공간 단위 권한(채널 덮어쓰기 적용 전).
    pub fn base_permissions(&self, user: &UserId) -> Permissions {
        self.member_base_permissions(user, self.members.get(user).map(|m| &**m))
    }

    /// 멤버 항목을 이미 쥔 쪽의 [`Self::base_permissions`].
    fn member_base_permissions(&self, user: &UserId, member: Option<&Member>) -> Permissions {
        if self.disbanded {
            return Permissions::NONE;
        }
        if *user == self.owner {
            return Permissions::ALL;
        }
        let Some(member) = member else {
            return Permissions::NONE;
        };
        let mut p = self
            .roles
            .get(&RoleId::EVERYONE)
            .map(|r| r.permissions)
            .unwrap_or_default();
        for rid in &member.roles {
            if let Some(r) = self.roles.get(rid) {
                p = p.union(r.permissions);
            }
        }
        if p.contains(Permissions::ADMINISTRATOR) {
            return Permissions::ALL;
        }
        p
    }

    /// 채널 덮어쓰기까지 적용한 권한(디스코드 규칙: @everyone → 역할들 → 멤버 순).
    pub fn channel_permissions(&self, user: &UserId, channel: &ChannelId) -> Permissions {
        self.permissions_in(
            user,
            self.members.get(user).map(|m| &**m),
            self.channels.get(channel),
        )
    }

    /// 멤버 항목(`member`)과 채널 설정(`ch`, 없는 채널은 `None`)을 이미 쥔 쪽의 [`Self::channel_permissions`]. 3만 명
    /// 크루에서 '채널을 누가 볼 수 있나'가 멤버마다 부르므로 표를 다시 찾거나 역할 목록을 복사하지 않습니다.
    pub fn permissions_in(
        &self,
        user: &UserId,
        member: Option<&Member>,
        ch: Option<&ChannelSettings>,
    ) -> Permissions {
        let base = self.member_base_permissions(user, member);
        if base == Permissions::ALL {
            return base;
        }
        // 해산한 크루는 채널 덮어쓰기의 허용으로도 권한이 생기지 않습니다.
        let Some(ch) = ch.filter(|_| !self.disbanded) else {
            return Permissions::NONE;
        };
        let member_roles: &[RoleId] = member.map(|m| m.roles.as_slice()).unwrap_or_default();
        let mut p = base;
        if let Some(o) = ch
            .overwrites
            .iter()
            .find(|o| o.target == OverwriteTarget::Role(RoleId::EVERYONE))
        {
            p = p.remove(o.deny).union(o.allow);
        }
        let mut allow = Permissions::NONE;
        let mut deny = Permissions::NONE;
        for o in &ch.overwrites {
            if let OverwriteTarget::Role(r) = o.target
                && r != RoleId::EVERYONE
                && member_roles.contains(&r)
            {
                allow = allow.union(o.allow);
                deny = deny.union(o.deny);
            }
        }
        p = p.remove(deny).union(allow);
        if let Some(o) = ch
            .overwrites
            .iter()
            .find(|o| o.target == OverwriteTarget::Member(*user))
        {
            p = p.remove(o.deny).union(o.allow);
        }
        if !p.contains(Permissions::VIEW_CHANNEL) {
            return Permissions::NONE;
        }
        p
    }

    /// 사용자의 최고 역할 순위(`Role::level`, 소유자는 u64::MAX).
    pub fn rank_of(&self, user: &UserId) -> u64 {
        if *user == self.owner {
            return u64::MAX;
        }
        self.members
            .get(user)
            .map(|m| {
                m.roles
                    .iter()
                    .filter_map(|r| self.roles.get(r))
                    .map(Role::level)
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0)
    }

    pub fn role_display(&self, role: &RoleId) -> RoleDisplay {
        self.role_display
            .get(role)
            .cloned()
            .unwrap_or_else(|| RoleDisplay::default_for(role))
    }

    /// 관리자: 소유자와 관리자 권한 역할을 가진 멤버.
    pub fn is_admin(&self, user: &UserId) -> bool {
        self.base_permissions(user) == Permissions::ALL
    }

    /// 역할 표시 설정과 상관없이 모든 역할과 관리자를 보는 사람(소유자·관리자·역할 관리자).
    fn sees_all_roles(&self, viewer: &UserId) -> bool {
        self.base_permissions(viewer)
            .contains(Permissions::MANAGE_ROLES)
    }

    /// 보는 사람이 가진 역할(@everyone 포함, 있는 역할만).
    fn held_roles(&self, viewer: &UserId) -> BTreeSet<RoleId> {
        let mut held = BTreeSet::from([RoleId::EVERYONE]);
        if let Some(m) = self.members.get(viewer) {
            held.extend(m.roles.iter().filter(|r| self.roles.contains_key(r)));
        }
        held
    }

    /// 보는 사람이 누가 관리자인지 봐도 되는지.
    pub fn sees_admins(&self, viewer: &UserId) -> bool {
        if self.sees_all_roles(viewer) {
            return true;
        }
        self.is_member(viewer)
            && self
                .held_roles(viewer)
                .iter()
                .any(|r| self.role_display(r).see_admins)
    }

    /// 보는 사람에게 보이는 역할. `None`이면 거르지 않음(모두 보임).
    pub fn visible_roles(&self, viewer: &UserId) -> Option<BTreeSet<RoleId>> {
        if self.sees_all_roles(viewer) {
            return None;
        }
        let held = self.held_roles(viewer);
        let admins = self.sees_admins(viewer);
        let visible: BTreeSet<RoleId> = self
            .roles
            .iter()
            .filter(|(id, r)| {
                if held.contains(id) {
                    return true;
                }
                if !admins && r.permissions.contains(Permissions::ADMINISTRATOR) {
                    return false;
                }
                match self.role_display(id).visible_to {
                    None => true,
                    Some(list) => list.iter().any(|x| held.contains(x)),
                }
            })
            .map(|(id, _)| *id)
            .collect();
        (visible.len() < self.roles.len()).then_some(visible)
    }

    /// P2P 전용 채널인지(내용이 Tor 동기화로 가지 않음).
    pub fn is_p2p(&self, channel: &ChannelId) -> bool {
        self.channel_transport.get(channel) == Some(&ChannelTransport::P2p)
    }

    /// 지금 익명 채널인지. 켜기 전에 쓴 글도 켜져 있는 동안은 익명으로 보입니다.
    pub fn is_anonymous(&self, channel: &ChannelId) -> bool {
        self.anonymous_channels.contains(channel)
    }

    /// 채널 첨부 하나의 한도(크루 관리자가 정함). `None`이면 무제한, `Some(0)`이면 첨부 금지.
    pub fn attach_limit(&self, channel: &ChannelId) -> Option<u64> {
        self.attach_limits.get(channel).copied()
    }

    /// 상시 휘발성 채널 참가 기기 상한.
    pub fn volatile_cap(&self, channel: &ChannelId) -> u32 {
        self.volatile_caps
            .get(channel)
            .copied()
            .unwrap_or(DEFAULT_VOLATILE_CAP)
    }

    /// 채널의 수정·삭제 금지(없으면 둘 다 꺼짐).
    pub fn edit_rules(&self, channel: &ChannelId) -> EditRules {
        self.edit_rules.get(channel).copied().unwrap_or_default()
    }

    /// 지금 올릴 수 있는 이모지 수 상한: 크루 한도와 해소기 상한([`MAX_EMOJIS`]) 가운데 작은 것.
    pub fn emoji_count_limit(&self) -> usize {
        self.emoji_limits
            .max_count
            .map_or(MAX_EMOJIS, |n| (n as usize).min(MAX_EMOJIS))
    }

    pub fn is_member(&self, user: &UserId) -> bool {
        self.members.contains_key(user)
    }

    /// 기기가 그 사용자의 활성 기기인지(상태에 기록된 기기 목록 기준).
    pub fn device_belongs(&self, user: &UserId, device: &DeviceId) -> bool {
        self.devices
            .get(user)
            .is_some_and(|l| l.contains_active(device))
    }

    /// 멤버의 활성 기기(멤버가 아니면 없음).
    pub fn member_active_devices(
        &self,
        user: &UserId,
    ) -> impl Iterator<Item = &crate::device::DeviceRecord> + '_ {
        self.devices
            .get(user)
            .filter(|_| self.is_member(user))
            .into_iter()
            .flat_map(|l| l.active())
    }

    /// `self`에서 `newer`로 가며 멤버 항목이나 기기 기록이 바뀐 사용자(정렬, 중복 없음). 지속 맵 차이라 나눠 쓰는 마디는
    /// 건너뛰어 바뀐 만큼만 돕니다.
    pub fn changed_members(&self, newer: &SpaceState) -> Vec<UserId> {
        fn key<'a, K, V>(d: DiffItem<'a, 'a, K, V>) -> &'a K {
            match d {
                DiffItem::Add(k, _) | DiffItem::Remove(k, _) => k,
                DiffItem::Update { new: (k, _), .. } => k,
            }
        }
        let mut users: Vec<UserId> = self
            .members
            .diff(&newer.members)
            .map(|d| *key(d))
            .chain(self.devices.diff(&newer.devices).map(|d| *key(d)))
            .collect();
        users.sort_unstable();
        users.dedup();
        users
    }

    /// 모든 멤버의 활성 기기(사용자 순). 복사 없이 빌려 주므로 3만 명 상태에서도 거르기만 하는 쪽은 할당하지 않습니다.
    /// 상자 키는 기록의 키 약속(`key_commit`)으로 기기 키 표에서 찾습니다.
    pub fn active_devices(
        &self,
    ) -> impl Iterator<Item = (&UserId, &crate::device::DeviceRecord)> + '_ {
        self.members_with_devices()
            .filter_map(|(u, _, l)| l.map(|l| (u, l)))
            .flat_map(|(u, l)| l.active().map(move |d| (u, d)))
    }

    /// 멤버 항목과 그 기기 기록(없으면 `None`)을 사용자 순으로. 두 표가 같은 열쇠 순서라 사용자마다 찾지 않고 나란히
    /// 걷습니다(3만 명에서 B-트리 찾기 3만 번 대신 한 번 훑기).
    pub fn members_with_devices(
        &self,
    ) -> impl Iterator<Item = (&UserId, &Member, Option<&crate::device::MemberDevices>)> + '_ {
        let mut devices = self.devices.iter().peekable();
        self.members.iter().map(move |(u, m)| {
            while devices.next_if(|(d, _)| *d < u).is_some() {}
            let l = devices.next_if(|(d, _)| *d == u).map(|(_, l)| &**l);
            (u, &**m, l)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> (SpaceState, UserId, UserId, ChannelId, RoleId) {
        let owner = UserId::random();
        let bob = UserId::random();
        let mods = RoleId::random();
        let ch = ChannelId::random();
        let mut roles = BTreeMap::new();
        roles.insert(
            RoleId::EVERYONE,
            Role::everyone(Permissions::DEFAULT_EVERYONE),
        );
        roles.insert(
            mods,
            Role {
                id: mods,
                name: "운영진".into(),
                color: 0,
                rank: 10,
                permissions: Permissions::KICK_MEMBERS,
                automation: false,
            },
        );
        let mut members = BTreeMap::new();
        members.insert(
            owner,
            Member {
                user: owner,
                roles: vec![],
                nickname: None,
                automation: false,
            },
        );
        members.insert(
            bob,
            Member {
                user: bob,
                roles: vec![],
                nickname: None,
                automation: false,
            },
        );
        let mut secret = ChannelSettings::text("비밀");
        secret.overwrites = vec![
            PermissionOverwrite {
                target: OverwriteTarget::Role(RoleId::EVERYONE),
                allow: Permissions::NONE,
                deny: Permissions::VIEW_CHANNEL,
            },
            PermissionOverwrite {
                target: OverwriteTarget::Role(mods),
                allow: Permissions::VIEW_CHANNEL,
                deny: Permissions::NONE,
            },
        ];
        let mut channels = SharedMap::new();
        channels.insert(ch, secret);
        (
            SpaceState {
                kind: SpaceKind::Crew,
                owner,
                committer: DeviceId::random(),
                settings: SpaceSettings {
                    name: "게임 모임".into(),
                    description: String::new(),
                    icon: None,
                    icon_key: None,
                    allow_external_emoji: false,
                    strict_rotation: false,
                },
                members: members.into(),
                roles,
                channels,
                devices: SharedMap::new(),
                invites: Default::default(),
                emojis: BTreeMap::new(),
                pending_crew_conversion: None,
                bans: SharedMap::new(),
                archives: BTreeMap::new(),
                channel_transport: BTreeMap::new(),
                role_display: SharedMap::new(),
                anonymous_channels: BTreeSet::new(),
                crew_servers: Default::default(),
                forum_featured: BTreeMap::new(),
                attach_limits: BTreeMap::new(),
                volatile_caps: BTreeMap::new(),
                edit_rules: BTreeMap::new(),
                emoji_limits: EmojiLimits::default(),
                emoji_by: BTreeMap::new(),
                disbanded: false,
            },
            owner,
            bob,
            ch,
            mods,
        )
    }

    #[test]
    fn private_channel_requires_role() {
        let (mut s, owner, bob, ch, mods) = state();
        assert!(s.channels[&ch].is_private());
        assert_eq!(s.channel_permissions(&owner, &ch), Permissions::ALL);
        assert_eq!(s.channel_permissions(&bob, &ch), Permissions::NONE);
        Arc::make_mut(s.members.get_mut(&bob).unwrap())
            .roles
            .push(mods);
        assert!(
            s.channel_permissions(&bob, &ch)
                .contains(Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES)
        );
        assert_eq!(s.rank_of(&bob), 10);
    }

    #[test]
    fn members_with_devices_matches_lookup_per_member() {
        let (mut s, owner, bob, ch, _) = state();
        let rec = |n: u8| {
            Arc::new(crate::device::MemberDevices {
                version: 1,
                devices: vec![crate::device::DeviceRecord {
                    device: DeviceId::random(),
                    added_at: crate::time::Timestamp::now(),
                    revoked_at: None,
                    ed_pk: [n; 32],
                    pq_hash: [n; 32],
                    key_commit: [n; 32],
                }],
                otp: None,
            })
        };
        // 멤버가 아닌 사용자(나간 멤버)의 기록과 기록 없는 멤버가 사이사이 끼어도 사용자마다 찾은 것과 같아야 합니다.
        for i in 0..40u8 {
            let u = UserId::random();
            if i % 2 == 0 {
                s.devices.insert(u, rec(i));
            } else {
                s.members.insert(
                    u,
                    Arc::new(Member {
                        user: u,
                        roles: vec![],
                        nickname: None,
                        automation: false,
                    }),
                );
                if i % 3 == 0 {
                    s.devices.insert(u, rec(i));
                }
            }
        }
        s.devices.insert(bob, rec(200));
        let walked: Vec<_> = s
            .members_with_devices()
            .map(|(u, m, l)| (*u, m.clone(), l.cloned()))
            .collect();
        let looked: Vec<_> = s
            .members
            .iter()
            .map(|(u, m)| (*u, (**m).clone(), s.devices.get(u).map(|l| (**l).clone())))
            .collect();
        assert_eq!(walked, looked);
        assert!(walked.iter().any(|(u, _, l)| *u == bob && l.is_some()));
        // 쥔 항목으로 계산한 권한도 찾아서 계산한 것과 같습니다.
        for (u, m, _) in s.members_with_devices() {
            assert_eq!(
                s.permissions_in(u, Some(m), s.channels.get(&ch)),
                s.channel_permissions(u, &ch)
            );
        }
        assert_eq!(s.channel_permissions(&owner, &ch), Permissions::ALL);
    }

    #[test]
    fn key_group_follows_view_rule_only() {
        let (mut s, owner, bob, ch, mods) = state();
        let space = SpaceId::random();
        let secret = s.channels[&ch].clone();
        let g = s.channel_key_group(&space, &secret).unwrap();
        // 키 갱신 형식과 같은 유도(비공개 그룹), 크루 그룹(공간 ID)과는 다름.
        assert_eq!(g, KeyGroupId::private(&space, &secret.view_rule()));
        assert_ne!(g, KeyGroupId::crew(&space));
        // 보기 비트가 아닌 덮어쓰기·이름·순서는 그룹을 바꾸지 않습니다.
        let mut twin = secret.clone();
        twin.name = "다른 이름".into();
        twin.overwrites.reverse();
        twin.overwrites.push(PermissionOverwrite {
            target: OverwriteTarget::Member(bob),
            allow: Permissions::NONE,
            deny: Permissions::SEND_MESSAGES,
        });
        assert_eq!(s.channel_key_group(&space, &twin), Some(g));
        // 보기 규칙이 다르면 다른 그룹, 다른 공간이면 다른 그룹.
        let mut allowed = ChannelSettings::text("모두 허용");
        allowed.overwrites.push(PermissionOverwrite {
            target: OverwriteTarget::Role(RoleId::EVERYONE),
            allow: Permissions::VIEW_CHANNEL,
            deny: Permissions::NONE,
        });
        let allowed_group = s.channel_key_group(&space, &allowed).unwrap();
        assert_ne!(allowed_group, g);
        assert_ne!(s.channel_key_group(&SpaceId::random(), &secret), Some(g));
        // 휘발성 채널은 키 그룹이 없습니다.
        let mut volatile = secret.clone();
        volatile.security = SecurityLevel::Volatile;
        assert_eq!(s.channel_key_group(&space, &volatile), None);
        // 보기 덮어쓰기가 없고 @everyone이 보는 채널은 모든 멤버가 보므로 크루 그룹만 씁니다(같은 사람들에게 ID 둘을
        // 두지 않음). @everyone이 볼 수 없게 되면 그런 채널도 볼 수 있는 역할의 비공개 그룹으로 옮깁니다.
        let public = ChannelSettings::text("공개");
        assert_eq!(s.channel_key_group(&space, &public), None);
        let mut closed = s.clone();
        let everyone = closed.roles.get_mut(&RoleId::EVERYONE).unwrap();
        everyone.permissions = everyone.permissions.remove(Permissions::VIEW_CHANNEL);
        let empty_rule = closed.channel_key_group(&space, &public).unwrap();
        assert_eq!(empty_rule, KeyGroupId::private(&space, &[]));
        assert_ne!(empty_rule, KeyGroupId::crew(&space));
        // 볼 수 있는 사람만 그 그룹 스트림을 받을 자격이 있습니다.
        assert!(s.has_key_group(&space, &g));
        assert!(s.can_view_key_group(&space, &g, &owner));
        assert!(!s.can_view_key_group(&space, &g, &bob));
        assert!(!s.can_view_key_group(&space, &g, &UserId::random()));
        Arc::make_mut(s.members.get_mut(&bob).unwrap())
            .roles
            .push(mods);
        assert!(s.can_view_key_group(&space, &g, &bob));
        assert!(!s.has_key_group(&space, &KeyGroupId::random()));
        // 공개 채널 규칙에서 나온 ID(빈 규칙)는 그 채널이 크루 그룹을 쓰는 동안 아무 채널의 그룹도 아닙니다.
        s.channels.insert(ChannelId::random(), public);
        assert!(!s.has_key_group(&space, &KeyGroupId::private(&space, &[])));
        assert!(!s.can_view_key_group(&space, &KeyGroupId::private(&space, &[]), &bob));
    }

    #[test]
    fn role_display_hides_roles_and_admins_on_screen_only() {
        let (mut s, owner, bob, _, mods) = state();
        let admins = RoleId::random();
        let mut admin_role = s.roles[&mods].clone();
        admin_role.id = admins;
        admin_role.rank = 20;
        admin_role.permissions = Permissions::ADMINISTRATOR;
        s.roles.insert(admins, admin_role);
        let carol = UserId::random();
        s.members.insert(
            carol,
            Member {
                user: carol,
                roles: vec![admins],
                nickname: None,
                automation: false,
            }
            .into(),
        );
        // 기본값: 모두 보임.
        assert_eq!(s.visible_roles(&bob), None);
        assert!(s.sees_admins(&bob));
        assert!(s.is_admin(&owner) && s.is_admin(&carol) && !s.is_admin(&bob));

        // 운영진은 운영진만 봄: bob에게는 안 보이고, 운영진을 가진 사람·소유자·관리자는 봄.
        s.role_display.insert(
            mods,
            RoleDisplay {
                visible_to: Some(vec![]),
                see_admins: true,
            },
        );
        let v = s.visible_roles(&bob).unwrap();
        assert!(!v.contains(&mods) && v.contains(&admins) && v.contains(&RoleId::EVERYONE));
        assert_eq!(s.visible_roles(&owner), None);
        assert_eq!(s.visible_roles(&carol), None);
        Arc::make_mut(s.members.get_mut(&bob).unwrap())
            .roles
            .push(mods);
        assert_eq!(s.visible_roles(&bob), None);

        // @everyone이 관리자를 못 보게 하면 관리자 권한 역할이 빠지고, 관리자 보기를 켠 역할을 가지면 다시 봄.
        s.role_display.insert(
            RoleId::EVERYONE,
            RoleDisplay {
                visible_to: None,
                see_admins: false,
            },
        );
        assert!(s.sees_admins(&bob), "운영진에 관리자 보기를 켜 둠");
        s.role_display.get_mut(&mods).unwrap().see_admins = false;
        assert!(!s.sees_admins(&bob));
        Arc::make_mut(s.members.get_mut(&bob).unwrap())
            .roles
            .clear();
        assert!(!s.sees_admins(&bob));
        let v = s.visible_roles(&bob).unwrap();
        assert!(!v.contains(&admins) && !v.contains(&mods));
        // 표시만 거릅니다: 권한 계산은 그대로.
        assert_eq!(s.base_permissions(&carol), Permissions::ALL);
    }

    #[test]
    fn parse_permission_names() {
        let p = Permissions::parse_list("manage_channels,kick").unwrap();
        assert!(p.contains(Permissions::MANAGE_CHANNELS));
        assert!(p.contains(Permissions::KICK_MEMBERS));
        assert!(Permissions::parse_list("nope").is_err());
        let ban = Permissions::parse_list("ban").unwrap();
        assert_eq!(ban, Permissions::BAN_MEMBERS);
        assert!(!Permissions::DEFAULT_EVERYONE.contains(Permissions::BAN_MEMBERS));
    }

    /// 전송 방식 표가 생기기 전의 상태 캐시(JSON)도 그대로 읽히고, 그 채널들은 Tor 채널입니다.
    #[test]
    fn state_json_without_channel_transport_still_loads() {
        let (mut s, _, _, ch, _) = state();
        let mut v = serde_json::to_value(&s).unwrap();
        assert!(
            v.as_object_mut()
                .unwrap()
                .remove("channel_transport")
                .is_some()
        );
        let old: SpaceState = serde_json::from_value(v).unwrap();
        assert_eq!(old, s);
        assert!(!old.is_p2p(&ch));

        s.channel_transport.insert(ch, ChannelTransport::P2p);
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["channel_transport"][ch.to_string()], "p2p");
        let back: SpaceState = serde_json::from_value(v).unwrap();
        assert!(back.is_p2p(&ch));
        assert!(!back.is_p2p(&ChannelId::random()));

        // 익명 채널 집합도 옛 캐시에는 없습니다.
        let mut v = serde_json::to_value(&s).unwrap();
        assert!(
            v.as_object_mut()
                .unwrap()
                .remove("anonymous_channels")
                .is_some()
        );
        let old: SpaceState = serde_json::from_value(v).unwrap();
        assert!(!old.is_anonymous(&ch));
        s.anonymous_channels.insert(ch);
        let back: SpaceState = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        assert!(back.is_anonymous(&ch));

        // 대문 표도 옛 캐시에는 없습니다.
        s.forum_featured.insert(ch, EventId([3; 32]));
        let mut v = serde_json::to_value(&s).unwrap();
        assert!(
            v.as_object_mut()
                .unwrap()
                .remove("forum_featured")
                .is_some()
        );
        let old: SpaceState = serde_json::from_value(v).unwrap();
        assert!(old.forum_featured.is_empty());
        let back: SpaceState = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        assert_eq!(back.forum_featured.get(&ch), Some(&EventId([3; 32])));
        s.forum_featured.clear();

        // 크루 서버 목록도 옛 캐시에는 없습니다.
        let mut v = serde_json::to_value(&s).unwrap();
        assert!(v.as_object_mut().unwrap().remove("crew_servers").is_some());
        let old: SpaceState = serde_json::from_value(v).unwrap();
        assert!(old.crew_servers.is_empty());
    }

    /// 무작위 값 없이 만든 상태(직렬화 바이트 비교용).
    fn fixed_state() -> SpaceState {
        use crate::device::{BoxPublicKey, DeviceEntry, DeviceList, DeviceSignKeys, MemberDevices};
        use crate::time::Timestamp;
        let (owner, bob) = (UserId([1; 32]), UserId([2; 32]));
        let mods = RoleId([3; 16]);
        let ch = ChannelId([4; 16]);
        let entry = |d: u8, revoked: Option<u64>| DeviceEntry {
            device: DeviceSignKeys::filled(d).device_id(),
            box_key: BoxPublicKey::filled(d),
            sign_keys: DeviceSignKeys::filled(d),
            name: format!("기기{d}"),
            added_at: Timestamp(1_000 + u64::from(d)),
            revoked_at: revoked.map(Timestamp),
        };
        let mut roles = BTreeMap::new();
        roles.insert(
            RoleId::EVERYONE,
            Role::everyone(Permissions::DEFAULT_EVERYONE),
        );
        roles.insert(
            mods,
            Role {
                id: mods,
                name: "운영진".into(),
                color: 0x00AB_CDEF,
                rank: 10,
                permissions: Permissions::KICK_MEMBERS,
                automation: false,
            },
        );
        let member = |user, roles, nickname: Option<&str>| Member {
            user,
            roles,
            nickname: nickname.map(Into::into),
            automation: false,
        };
        let members = [
            (owner, member(owner, vec![], None)),
            (bob, member(bob, vec![mods], Some("밥"))),
        ]
        .into_iter()
        .collect();
        let list = |user, version, devices| {
            Arc::new(MemberDevices::from_list(&DeviceList {
                user,
                version,
                devices,
                otp: None,
            }))
        };
        let devices = [
            (
                owner,
                list(owner, 3, vec![entry(5, None), entry(6, Some(2_000))]),
            ),
            (bob, list(bob, 1, vec![entry(7, None)])),
        ]
        .into_iter()
        .collect();
        let mut bans = SharedMap::new();
        bans.insert(
            UserId([8; 32]),
            BanInfo {
                reason: "도배".into(),
                banned_by: owner,
                banned_at: Timestamp(3_000),
            },
        );
        // 채널·초대·역할 표시도 채워, 지속 자료 구조로 바꾼 표가 여러 마디(16항목마다)일 때도 같은지 봅니다.
        let mut forum = ChannelSettings::text("포럼");
        forum.kind = ChannelKind::Forum;
        forum.topic = "주제".repeat(10);
        forum.overwrites = vec![
            PermissionOverwrite {
                target: OverwriteTarget::Role(mods),
                allow: Permissions::MANAGE_MESSAGES,
                deny: Permissions::NONE,
            },
            PermissionOverwrite {
                target: OverwriteTarget::Member(bob),
                allow: Permissions::NONE,
                deny: Permissions::SEND_MESSAGES,
            },
        ];
        forum.forum_tags = vec![ForumTag {
            id: 1,
            name: "질문".into(),
            emoji: Some("?".into()),
        }];
        let channels = std::iter::once((ch, ChannelSettings::text("일반")))
            .chain((0..20u8).map(|i| {
                let mut s = if i == 7 {
                    forum.clone()
                } else {
                    ChannelSettings::text(format!("채널{i}"))
                };
                s.position = i32::from(i);
                (ChannelId([40 + i; 16]), s)
            }))
            .collect();
        let invites: Vec<InviteState> = (0..3u8)
            .map(|i| InviteState {
                info: crate::body::InviteInfo {
                    token_hash: [i; 32],
                    verify_key: [i + 1; 32],
                    expires_at: (i == 1).then_some(Timestamp(9_000)),
                    max_uses: Some(u32::from(i) + 1),
                    roles: vec![mods],
                },
                created_by: owner,
                uses: u32::from(i),
            })
            .collect();
        let role_display = [
            (
                mods,
                RoleDisplay {
                    visible_to: Some(vec![mods]),
                    see_admins: true,
                },
            ),
            (
                RoleId::EVERYONE,
                RoleDisplay {
                    visible_to: None,
                    see_admins: false,
                },
            ),
        ]
        .into_iter()
        .collect();
        SpaceState {
            kind: SpaceKind::Crew,
            owner,
            committer: DeviceSignKeys::filled(5).device_id(),
            settings: SpaceSettings {
                name: "고정 상태".into(),
                description: "직렬화 비교".into(),
                icon: None,
                icon_key: None,
                allow_external_emoji: true,
                strict_rotation: false,
            },
            members,
            roles,
            channels,
            devices,
            invites: invites.into_iter().collect(),
            emojis: BTreeMap::new(),
            pending_crew_conversion: None,
            bans,
            archives: BTreeMap::new(),
            channel_transport: BTreeMap::new(),
            role_display,
            anonymous_channels: BTreeSet::from([ch]),
            crew_servers: Default::default(),
            forum_featured: BTreeMap::new(),
            attach_limits: BTreeMap::from([(ch, 1 << 20)]),
            volatile_caps: BTreeMap::new(),
            edit_rules: BTreeMap::new(),
            emoji_limits: EmojiLimits {
                max_count: Some(10),
                max_bytes: None,
            },
            emoji_by: BTreeMap::new(),
            disbanded: false,
        }
    }

    /// 지속 자료 구조는 여러 마디(16·64항목마다)로 커져도 표준 자료 구조와 같은 바이트로 직렬화되고 그대로 되읽힙니다.
    #[test]
    fn shared_containers_encode_like_std() {
        let pairs: Vec<(ChannelId, ChannelSettings)> = (0..500u16)
            .map(|i| {
                let mut b = [0u8; 16];
                b[..2].copy_from_slice(&i.to_le_bytes());
                (ChannelId(b), ChannelSettings::text(format!("채널{i}")))
            })
            .collect();
        let std_map: BTreeMap<ChannelId, ChannelSettings> = pairs.iter().cloned().collect();
        let shared: SharedMap<ChannelId, ChannelSettings> = pairs.into_iter().collect();
        let bin = postcard::to_stdvec(&std_map).unwrap();
        let json = serde_json::to_vec(&std_map).unwrap();
        assert_eq!(postcard::to_stdvec(&shared).unwrap(), bin);
        assert_eq!(serde_json::to_vec(&shared).unwrap(), json);
        assert_eq!(
            postcard::from_bytes::<SharedMap<_, _>>(&bin).unwrap(),
            shared
        );
        assert_eq!(
            serde_json::from_slice::<SharedMap<_, _>>(&json).unwrap(),
            shared
        );
        let list: Vec<u64> = (0..1000).map(|i| i * 7).collect();
        let vector: imbl::Vector<u64> = list.iter().copied().collect();
        let bin = postcard::to_stdvec(&list).unwrap();
        assert_eq!(postcard::to_stdvec(&vector).unwrap(), bin);
        assert_eq!(
            serde_json::to_vec(&vector).unwrap(),
            serde_json::to_vec(&list).unwrap()
        );
        assert_eq!(
            postcard::from_bytes::<imbl::Vector<u64>>(&bin).unwrap(),
            vector
        );
    }

    /// 상태의 JSON(로컬 API)과 이진(postcard) 직렬화 바이트가 그대로인지. 기기를 간추린 기록으로 바꾼 형식(저장 형식 5판)에
    /// 키 갱신 2부의 `SpaceSettings::strict_rotation`을 더한 것(이벤트 형식 3판과 함께, 출시 전이라 호환 없음)에서 잰
    /// 길이·해시와 견줍니다. 상태 체크포인트는 공간 상태 형식을 바꾸지 않아 합친 뒤에도 같습니다. 10단계의 `volatile_caps`(상시 휘발성 채널 참가 상한)가 더해져 다시 쟀습니다(출시 전이라 옛 상태 행 호환 없음). 이모지 한도·올린 사람(`emoji_limits`·`emoji_by`)이 더해져 또 쟀습니다(옛 행은 데몬이 읽지 못하면 다시 해소해 씀). 크루 해산 표시(`disbanded`)는 직렬화하지 않아 그대로입니다. 바뀌면 저장된 상태 행을 다시 읽지 못하므로 일부러 바꿀 때만 고칩니다.
    #[test]
    fn state_encoding_is_unchanged() {
        let st = fixed_state();
        let json = serde_json::to_vec(&st).unwrap();
        let bin = postcard::to_stdvec(&st).unwrap();
        let digest = |b: &[u8]| blake3::hash(b).to_hex().to_string();
        assert_eq!(
            (json.len(), digest(&json).as_str()),
            (
                8741,
                "569e53cd71d22ea2a9c2a7020a66a0e3c8f77dcf6388a92bd80d50435491fb15"
            )
        );
        assert_eq!(
            (bin.len(), digest(&bin).as_str()),
            (
                2273,
                "ee0d4c32d5f4653e4bae90b91451c4c80e84da6ad84abb1344c764d41dfbcdff"
            )
        );
        assert_eq!(serde_json::from_slice::<SpaceState>(&json).unwrap(), st);
        assert_eq!(postcard::from_bytes::<SpaceState>(&bin).unwrap(), st);
    }
}
