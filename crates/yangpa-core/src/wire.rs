//! 노드 사이 요청-응답 프로토콜(`/yangpa/rpc/5`, OpenZL로 압축한 CBOR)과 gossip 메시지.
//!
//! 연결 직후 먼저 연결한 쪽이 [`YpRequest::Auth`]로 기기를 증명하고, 받는 쪽은 상대가
//! 친구·공간 멤버의 기기일 때만 자기 [`PeerAuth`]로 답합니다. 링크 사용(친구 추가·초대
//! 가입)을 빼면 인증 전 요청은 모두 거절합니다.

use serde::{Deserialize, Serialize};

use crate::badge::{FounderGrant, SignedBadgeCard};
use crate::blob::BlobManifest;
use crate::body::{InviteProof, Secret32};
use crate::checkpoint::{CheckpointPin, DeviceLeafProof, SignedCheckpoint};
use crate::device::{DeviceEntry, DeviceKeys, SignedDeviceList, SignedProfile};
use crate::error::CoreError;
use crate::event::{SignedEvent, StreamId};
use crate::ids::{
    BlobId, CallId, ChannelId, ChannelRootId, DeviceId, EventId, KeyGroupId, LinkToken, SpaceId,
    StateKeyId, UserId,
};
use crate::sign::{Sig, SignDomain, SigningKeypair, verify};
use crate::time::Timestamp;

/// 2판: 요청·응답을 OpenZL로 압축해 보냄(`yangpa-zl`). 1판과는 서로 알아듣지 못해 프로토콜 이름을 바꿈.
/// 3판: 기기 목록 항목에 서명 공개키가 실려(인증·초대·기기 목록 전달) 2판 노드가 풀지 못함.
/// 4판: 이벤트 형식 3판(키 표시 변형이 바뀜)이라 3판 노드와 이벤트를 주고받으면 서로 잘못한 피어로 셈. 같은 판에서
/// 키 그룹 요청(`GetChannelRoots`…`GetKeyShard`), 체크포인트 요청(`GetDeviceKeys`·`GetCheckpoint`), 그룹 멤버 요청
/// (`GetGroupMembers`), 휘발성 키 감싸기(`VolatileKeyWrap`)가 이 순서로 붙음.
/// 5판: 채널 이벤트의 짧은 서명(4판 노드는 서명 길이에서 거절)과 공간 상태 형식(상시 참여 상한)이 바뀜. 같은 판에서
/// 채널 끝 묻기(`GetChannelTips`), 멤버 소개(`FindDevice`), 멤버 노드 목록(`GetMemberNodes`), 놓친 가입 확인
/// (`CheckJoin`)이 이 순서로 붙음(모르는 노드에게서는 답을 받지 못할 뿐).
pub const RPC_PROTOCOL: &str = "/yangpa/rpc/5";
/// 요청·응답 하나의 최대 크기(압축 전 CBOR 기준).
pub const MAX_WIRE_MESSAGE: usize = 8 * 1024 * 1024;
/// 한 번에 주고받는 이벤트 최대 개수.
pub const MAX_EVENTS_PER_MESSAGE: usize = 512;

/// 기기 증명: 이 libp2p 연결의 양 끝 PeerId에 묶인 서명.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerAuth {
    pub user: UserId,
    pub device: DeviceId,
    pub devices: SignedDeviceList,
    pub at: Timestamp,
    pub sig: Sig,
}

impl PeerAuth {
    fn message(
        user: &UserId,
        device: &DeviceId,
        local_peer: &[u8],
        remote_peer: &[u8],
        at: Timestamp,
    ) -> Vec<u8> {
        postcard::to_stdvec(&(user, device, local_peer, remote_peer, at)).expect("직렬화 실패 없음")
    }

    pub fn create(
        device_key: &SigningKeypair,
        devices: SignedDeviceList,
        local_peer: &[u8],
        remote_peer: &[u8],
    ) -> PeerAuth {
        let user = devices.list.user;
        let device = DeviceId(device_key.public());
        let at = Timestamp::now();
        let sig = device_key.sign(
            SignDomain::PeerAuth,
            &Self::message(&user, &device, local_peer, remote_peer, at),
        );
        PeerAuth {
            user,
            device,
            devices,
            at,
            sig,
        }
    }

    /// `sender_peer`는 상대(서명한 쪽)의 PeerId, `receiver_peer`는 내 PeerId.
    pub fn verify(
        &self,
        sender_peer: &[u8],
        receiver_peer: &[u8],
        max_skew_ms: u64,
    ) -> Result<(), CoreError> {
        self.devices.verify()?;
        if self.devices.list.user != self.user || !self.devices.list.contains_active(&self.device) {
            return Err(CoreError::BadSignature);
        }
        let now = Timestamp::now().0;
        if self.at.0.abs_diff(now) > max_skew_ms {
            return Err(CoreError::Expired);
        }
        let msg = Self::message(
            &self.user,
            &self.device,
            sender_peer,
            receiver_peer,
            self.at,
        );
        verify(
            self.device.as_bytes(),
            SignDomain::PeerAuth,
            &msg,
            &self.sig,
        )
    }
}

/// 친구 링크 사용 요청. 링크를 받은 사람이 만든 사람 노드로 보냅니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendRedeem {
    pub token: LinkToken,
    pub auth: PeerAuth,
    pub profile: SignedProfile,
}

/// 크루 초대 링크 사용 요청. 토큰 원문 대신 가입 증명을 보냅니다(받아 주는 멤버도 토큰을 알 필요가 없음).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteRedeem {
    pub space: SpaceId,
    pub proof: InviteProof,
    pub auth: PeerAuth,
    pub profile: SignedProfile,
}

/// 논리 시계 구간 [start, end).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LamportRange {
    pub start: u64,
    pub end: u64,
}

/// 구간 지문: 구간 안 이벤트 ID들의 XOR과 개수. 다르면 하위 구간으로 나눠 다시 비교합니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RangeFingerprint {
    pub range: LamportRange,
    pub count: u32,
    pub xor: [u8; 32],
}

/// WebRTC 연결 협상 메시지. 시그널링 서버 없이 libp2p 스트림으로 주고받습니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignalPayload {
    /// 통화 참가 알림(채널을 볼 수 있는 기기 모두에게). 이미 참가 중인 기기는 새 참가자에게만 `reply: true`로
    /// 자기 상태를 돌려줍니다(그 답에는 다시 답하지 않음). 데몬끼리만 주고받고 앱에는 참가자 목록으로 알립니다.
    Join {
        #[serde(default)]
        muted: bool,
        #[serde(default)]
        deafened: bool,
        #[serde(default)]
        screen: bool,
        /// 보내는 기기가 가진 프레임 키 세대(새 키 관리 기기가 그보다 큰 세대로 시작하게).
        #[serde(default)]
        generation: u64,
        #[serde(default)]
        reply: bool,
        /// SFU 호스트를 맡을 수 있으면 그 조건(못 맡으면 `None`). 키 관리 기기가 호스트를 고를 때 씁니다.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sfu: Option<SfuCaps>,
        /// 이 참가의 무작위 번호(참가할 때마다 새로, 다시 알릴 때는 그대로). 이미 참가자인 기기가 다른 번호로 알리면 새로
        /// 참가한 것(앱을 다시 켬)이라 받는 쪽은 지난 연결을 버립니다. 0은 모름(예전 빌드).
        #[serde(default)]
        session: u64,
    },
    Leave,
    Offer {
        sdp: String,
    },
    Answer {
        sdp: String,
    },
    Ice {
        candidate: String,
        #[serde(default)]
        sdp_mid: Option<String>,
        #[serde(default)]
        sdp_mline_index: Option<u16>,
    },
    /// 화면공유 '보기'를 누른 시청자 목록 변경.
    WatchScreen {
        watching: bool,
    },
    State {
        muted: bool,
        deafened: bool,
        screen: bool,
        /// 바뀐 SFU 호스트 조건(`Join`과 같음).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sfu: Option<SfuCaps>,
    },
    /// 프레임 E2EE 키 배포: 키 관리 기기가 세대마다 새 키를 받는 기기의 봉인 상자 키로 봉인해 보냅니다
    /// (protocol.md "음성·화면공유"). 데몬끼리만 주고받고 앱에는 보내지 않습니다.
    MediaKey {
        generation: u64,
        #[serde(with = "serde_bytes")]
        sealed: Vec<u8>,
    },
    /// SFU 세션 협상 제안. 앱 → 호스트면 새 세션(올리는 트랙 표 `tracks`, `device` 없음), 호스트 → 앱이면 같은 세션의
    /// 재협상(받을 트랙이 바뀜, `tracks`는 지금 보내는 트랙 전부와 그 출처 기기).
    SfuOffer {
        session: u32,
        sdp: String,
        #[serde(default)]
        tracks: Vec<SfuTrack>,
    },
    /// SFU 세션 협상 응답(방향은 `SfuOffer`의 반대).
    SfuAnswer {
        session: u32,
        sdp: String,
        #[serde(default)]
        tracks: Vec<SfuTrack>,
    },
    /// 앱 → 호스트 ICE 후보(호스트 후보는 SDP에 들어 있음).
    SfuIce {
        session: u32,
        candidate: String,
        #[serde(default)]
        sdp_mid: Option<String>,
        #[serde(default)]
        sdp_mline_index: Option<u16>,
    },
    /// 받는 쪽의 선택: 참가자 `device`의 `source` 트랙을 받을지, 어느 simulcast 층으로 받을지.
    SfuSelect {
        device: DeviceId,
        source: TrackSource,
        layer: SfuLayer,
    },
    /// SFU 세션을 끝냄(앱 → 호스트: 나감, 호스트 → 앱: 호스트가 더는 받지 않음).
    SfuClose {
        session: u32,
    },
    /// 통화 토폴로지 결정(키 관리 기기 → 참가자·호스트 후보). `host`가 있으면 SFU, 없으면 메시입니다. 받는 쪽은 자기
    /// 참가자 목록의 키 관리 기기가 보낸, 그 기기에게서 받은 것보다 큰 `seq`만 씁니다. 데몬끼리만.
    SfuPlan {
        seq: u64,
        host: Option<DeviceId>,
    },
    /// 통화에 참가하지 않고 SFU만 맡는 노드(크루가 운영하는 헤드리스 시더 등)의 알림. `None`이면 그만둠. 데몬끼리만.
    SfuHost {
        caps: Option<SfuCaps>,
    },
}

/// 통화 미디어 트랙의 출처(SFU가 받는 사람마다 무엇을 보낼지 정함).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackSource {
    Audio,
    Camera,
    Screen,
}

/// SFU 세션의 m-line과 트랙의 대응.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SfuTrack {
    pub mid: String,
    /// 그 트랙을 올린 참가자(호스트가 보내는 표에만).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<DeviceId>,
    pub source: TrackSource,
}

/// 받는 쪽이 고르는 simulcast 층. `Auto`는 호스트가 받는 쪽 대역폭 추정으로 고릅니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SfuLayer {
    #[default]
    Auto,
    High,
    Medium,
    Low,
    /// 받지 않음(화면공유 '보기'를 끔).
    Off,
}

/// SFU 호스트 후보의 조건. 호스트 선출에만 쓰고 앱에는 보내지 않습니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SfuCaps {
    /// 호스트를 맡겠다고 설정한 노드(헤드리스 시더 기본). 다른 후보보다 먼저 고릅니다.
    #[serde(default)]
    pub preferred: bool,
    /// 공인 주소로 닿음(AutoNAT 확인).
    #[serde(default)]
    pub public: bool,
    /// 통화 참가자 노드 중 릴레이로만 닿는 수.
    #[serde(default)]
    pub relayed: u16,
    /// 참가자 노드까지 왕복 시간 중앙값(ms, 모르면 `None`).
    #[serde(default)]
    pub rtt_ms: Option<u32>,
    /// 받을 수 있는 참가자 수.
    #[serde(default)]
    pub capacity: u16,
}

/// 친구·같은 공간 멤버·내 다른 기기 사이의 신원 정보 교환. 인증한 피어만 보낼 수 있습니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SocialRequest {
    /// 바뀐 서명 프로필(받는 쪽은 아는 사용자 것만, 더 높은 버전일 때만 씀). 이미지 키까지 실려 커서 `Box`
    /// (요청 열거형과 그것을 싣는 네트워크 이벤트가 커지지 않게. 직렬화 형식은 같음).
    Profile(Box<SignedProfile>),
    /// 보낸 기기 사용자의 최신 기기 목록. 받는 쪽은 이어받는 목록이면 저장하고 함께 있는
    /// 공간에 `DeviceListUpdate`로 전하며, 아는 가장 새 목록과 함께 있는 공간으로 답합니다([`SocialReply`]). 공동
    /// 서명까지 실려 커서 `Box`(직렬화 형식은 같음).
    ///
    /// `spaces`·`contacts`는 내 기기끼리만 싣습니다(보낸 기기가 멤버인 공간과 시작 이벤트, 연락처 기록). 답의 같은 칸과 짝이라
    /// 어느 쪽이 연결을 걸었든 서로 받습니다. 옛 빌드는 이 칸을 모르고 무시합니다.
    Devices {
        devices: Box<SignedDeviceList>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        spaces: Vec<(SpaceId, EventId)>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        contacts: Vec<OwnContact>,
        /// 내 기기끼리만: 끊어서 지운 1:1 공간. 받는 기기도 지우고 다시 받아들이지 않습니다.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        cut: Vec<SpaceId>,
        /// 내 기기끼리만: 보낸 기기가 휴대폰인지(서명 기기 목록에는 칸을 더할 수 없음).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        mobile: bool,
    },
    /// 보낸 사람이 친구를 끊고 이 1:1 공간을 지움. 받는 쪽도 공간과 연락처를 지웁니다.
    Cut { space: SpaceId },
    /// 내 기기끼리만: 받는 기기에게 봉인한 개인 메모 묶음.
    Notes {
        #[serde(with = "serde_bytes")]
        sealed: Vec<u8>,
    },
    /// 보낸 노드에 닿는 링크용 주소(onion·릴레이 경유, 자기 IP 없음, 최대 [`MAX_CONTACT_ADDRS`]개). identify가 없는
    /// Tor에서도 상대가 내 지금 PeerId의 주소를 남겨 다음에 다시 걸 수 있게 합니다. 답의 [`SocialReply::addrs`]에 답한
    /// 쪽 주소가 옵니다.
    ///
    /// `nodes`는 친구·내 기기 사이에만 싣는 보낸 기기의 노드 카드입니다([`NodeContact`], 지금은 Tor 노드 하나). 옛
    /// 빌드는 이 칸을 모르고 무시합니다.
    Contact {
        addrs: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        nodes: Vec<NodeContact>,
    },
    /// 임시 연락처끼리 친구 추가 요청(받는 쪽도 이미 요청했거나 친구면 바로 친구, [`SocialReply::friends`]). 같은
    /// 요청을 받은 쪽이 수락할 때도 이것을 보냅니다.
    Propose,
    /// 보낸 쪽이 나와의 임시 연락처를 만료함(링크 폐기·만료). 받는 쪽도 만료합니다(친구면 무시).
    ContactExpired,
    /// 1:1 공간을 크루로 바꾼 뒤 보낸 쪽이 새로 만든 둘만의 1:1 공간(공간 스트림 첫 이벤트, `FriendAccepted`와 같은
    /// 묶음). 받는 쪽이 이 공간을 쓰게 되면 `Ok`, 이미 다른 1:1 공간을 쓰면 그 공간을 [`SocialReply::friend_space`]로
    /// 답합니다. 옛 빌드는 이 요청을 모릅니다.
    DmSpace {
        space: SpaceId,
        events: Vec<SignedEvent>,
    },
}

/// 연락처 링크로 맺은 임시 연락처의 조건.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactTerms {
    /// 이때 임시 연락처가 만료됩니다(쓴 링크의 만료). 없으면 친구가 되거나 지울 때까지.
    #[serde(default)]
    pub expires_at: Option<Timestamp>,
}

/// 연락처 종류.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ContactKind {
    #[default]
    Friend,
    /// 연락처 링크로 만난 사람(친구 추가 전).
    Temporary,
    /// 링크가 만료돼 비활성화된 임시 연락처(지난 대화는 읽기만).
    Expired,
}

impl ContactKind {
    /// 저장소에 쓰는 번호.
    pub fn code(self) -> i64 {
        match self {
            ContactKind::Friend => 0,
            ContactKind::Temporary => 1,
            ContactKind::Expired => 2,
        }
    }

    pub fn from_code(c: i64) -> Self {
        match c {
            1 => ContactKind::Temporary,
            2 => ContactKind::Expired,
            _ => ContactKind::Friend,
        }
    }
}

/// 그 연락처가 보낸 친구 추가 요청의 상태([`OwnContact::request`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum OwnRequest {
    #[default]
    None,
    /// 받아서 답을 기다림.
    Pending,
    /// 거절함(꺼져 있던 내 기기의 기다림 기록이 되살리지 못하게 남김).
    Rejected,
}

/// 내 기기끼리 나누는 연락처 기록(`SocialRequest::Devices`, [`SocialReply::contacts`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnContact {
    pub user: UserId,
    /// 그 사람과의 1:1 공간.
    pub space: SpaceId,
    pub kind: ContactKind,
    #[serde(default)]
    pub expires_at: Option<Timestamp>,
    /// 그 연락처를 만든 연락처 링크 토큰의 해시.
    #[serde(default)]
    pub link_hash: Option<[u8; 32]>,
    /// 그 사람의 서명 프로필(그 사람과 연결된 적 없는 기기에도 이름이 보이게).
    #[serde(default)]
    pub profile: Option<SignedProfile>,
    /// 그 사람이 보낸 친구 추가 요청. 상대는 닿는 기기 하나에만 보내므로 내 기기끼리 맞춥니다.
    #[serde(default)]
    pub request: OwnRequest,
}

/// `Social(Contact)`에 싣는 주소 수 상한.
pub const MAX_CONTACT_ADDRS: usize = 8;
/// `Social(Contact)`에 싣는 노드 수 상한(옛 빌드는 모드마다 하나씩 보냈음, 여유를 둠).
pub const MAX_NODE_CONTACTS: usize = 4;

/// 기기의 노드 하나(PeerId)와 그 노드가 연결받는 onion 주소(`xxxx.onion:port`). 친구·내 기기에게만 줍니다(링크·위치
/// 기록에는 넣지 않음). 옛 빌드는 일반 모드 노드(친구 전용 onion)도 함께 보냈습니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeContact {
    #[serde(with = "serde_bytes")]
    pub peer_id: Vec<u8>,
    #[serde(default)]
    pub onion: Option<String>,
}

/// 모더레이션 직접 전달. 신고는 운영진(추방·밴·메시지 관리 권한이 있는 멤버)의 활성 기기마다 따로 봉인해
/// 보내므로 다른 멤버는 신고가 있었는지조차 모릅니다. 인증한 같은 공간 멤버만 보낼 수 있습니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModerationRequest {
    /// 새 신고(받는 기기의 상자 키로 봉인한 신고 내용).
    Report {
        space: SpaceId,
        #[serde(with = "serde_bytes")]
        sealed: Vec<u8>,
    },
    /// 다른 운영진에게 알리는 처리 결과(신고한 사람에게는 보내지 않음).
    Resolved {
        space: SpaceId,
        #[serde(with = "serde_bytes")]
        sealed: Vec<u8>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum YpRequest {
    Auth(Box<PeerAuth>),
    RedeemFriendLink(Box<FriendRedeem>),
    /// 친구 요청이 수락됨을 알림: 링크 토큰과 새 공간의 초기 이벤트(요청한 기기까지 봉인한 첫 크루 키 갱신 포함), 수락한
    /// 쪽의 서명된 프로필.
    FriendAccepted {
        token: LinkToken,
        space: SpaceId,
        events: Vec<SignedEvent>,
        /// 요청한 쪽은 링크만으로는 상대 프로필을 모르므로 함께 보냅니다(없던 이전 형식도 받음).
        /// 상자에 담아 요청 열거형이 커지지 않게 합니다(직렬화 형식은 같음).
        #[serde(default)]
        profile: Option<Box<SignedProfile>>,
        /// 연락처 링크로 맺은 임시 연락처면 그 조건(링크 주인이 정함). 없으면 친구(예전 친구 링크).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        contact: Option<ContactTerms>,
    },
    RedeemInvite(Box<InviteRedeem>),
    /// 실시간 전파(스페이스·소규모 공간은 직접 스트림으로).
    PushEvents {
        space: SpaceId,
        events: Vec<SignedEvent>,
    },
    GetHeads {
        space: SpaceId,
        stream: StreamId,
    },
    GetFingerprints {
        space: SpaceId,
        stream: StreamId,
        ranges: Vec<LamportRange>,
    },
    GetIds {
        space: SpaceId,
        stream: StreamId,
        range: LamportRange,
        limit: u32,
    },
    GetEvents {
        space: SpaceId,
        ids: Vec<EventId>,
    },
    BlobManifest {
        space: SpaceId,
        blob: BlobId,
    },
    /// 가진 조각 비트맵 요청.
    BlobHave {
        space: SpaceId,
        blob: BlobId,
    },
    BlobChunk {
        space: SpaceId,
        blob: BlobId,
        index: u32,
    },
    Signal {
        space: SpaceId,
        channel: ChannelId,
        call: CallId,
        from: DeviceId,
        payload: SignalPayload,
    },
    /// 늦게 입장한 사용자: 지금 RAM에 있고 수명이 남은 휘발성 메시지 요청.
    VolatileSnapshot {
        space: SpaceId,
        channel: ChannelId,
    },
    /// 휘발성 채널 입장·퇴장 알림.
    VolatilePresence {
        space: SpaceId,
        channel: ChannelId,
        present: bool,
    },
    /// 프로필·기기 목록·메모 교환([`SocialRequest`]).
    Social(SocialRequest),
    /// 신고·처리 결과 전달([`ModerationRequest`]).
    Moderation(ModerationRequest),
    Ping,
    /// 보관 전용 노드와 주고받는 요청([`crate::archive::ArchiveRequest`]). 지정·해제는 인증 전에도 받습니다.
    Archive(Box<crate::archive::ArchiveRequest>),
    /// P2P 안내(참여 채널·신뢰, Tor로 인증된 멤버 기기끼리만). 답은 받는 쪽의 안내([`YpResponse::P2pHello`]).
    P2pHello(Box<P2pHello>),
    /// 홀펀칭 맞추기(Tor로): 보내는 쪽의 막 고친 직접 스웜 노드·주소를 싣습니다. 받는 쪽은 자기 것을 답에 싣고
    /// 곧바로 받는 쪽 역할로 걸고(공유기 길 열기), 보낸 쪽은 답을 받자마자 겁니다([`YpResponse::P2pPunch`]).
    P2pPunch(Box<P2pHello>),
    /// 파일 회로 잇기(Tor): 보내는 기기의 파일 스웜(임시 키, 받지 않음)이 그 PeerId를 기기 키로 서명한 인증을 싣습니다.
    /// 받는 쪽은 같은 기기가 이미 주 스웜으로 인증돼 있을 때만 받아 들이고, 이 연결에서는 블롭 요청만 답합니다. 답은
    /// [`YpResponse::Ok`]. 파일 조각을 글과 다른 Tor 회로로 날라 글이 조각 뒤에 밀리지 않게 합니다.
    FileLink(Box<PeerAuth>),
    /// 이 채널에 글을 쓰는 중(저장하지 않는 일회성 알림, 몇 초마다 다시 보냄). 답은 [`YpResponse::Ok`].
    Typing {
        space: SpaceId,
        channel: ChannelId,
    },
    /// 모르는 채널 뿌리를 같은 공간 멤버에게 묻습니다(따라잡은 기록의 `KeyRef::Record`). 답은 요청한 기기에게 봉인한
    /// 키([`YpResponse::SealedKeys`]). 답하는 쪽은 메시지 시더와 같은 순서(활성 멤버 기기 → 채널 보기 → 기록 공개, 아니면
    /// `HistoryPrivate` → 가진 뿌리)로 거르고, 받는 쪽은 ID가 뿌리에서 나온 것만 씁니다. ID는 [`MAX_ROOT_IDS`]개까지.
    GetChannelRoots {
        space: SpaceId,
        channel: ChannelId,
        ids: Vec<ChannelRootId>,
    },
    /// 놓친 키 그룹의 지금 T·R을 그 그룹 사슬 끝 멤버에게 묻습니다(묶음을 못 받은 기기). 답은 봉인한 키이고, 받는 쪽은
    /// 사슬 끝의 뿌리 약속과 맞을 때만 씁니다.
    GetGroupKeys {
        space: SpaceId,
        group: KeyGroupId,
    },
    /// 옛 확정 기기가 새 확정 기기에게 키 그룹 표를 넘깁니다(`yangpa_crypto::keytree::seal_handover`, 서명 영역
    /// `KeyTable`). 답은 [`YpResponse::Ok`].
    KeyTableHandover(Box<KeyTableHandover>),
    /// 열지 못한 상태 이벤트·크루 갱신의 크루 상태 키를 같은 공간 멤버에게 묻습니다(가입 묶음은 최근 키 몇 개만 실어
    /// 크루 나이와 상관없는 크기). 답은 요청한 기기에게 봉인한 키([`YpResponse::SealedKeys`]). 답하는 쪽은 요청 기기가
    /// 활성 멤버 기기일 때만 가진 크루 상태 키를 싣고(그룹 상태 키는 싣지 않음), 받는 쪽은 키에서 계산한 ID가 물은 ID인
    /// 것만 씁니다. ID는 [`MAX_STATE_KEY_IDS`]개까지.
    GetStateKeys {
        space: SpaceId,
        ids: Vec<StateKeyId>,
    },
    /// 크루 전체 재키의 조각 블롭에서 요청한 기기의 몫만 받습니다(64KiB 조각 대신 몇 KB). 답은
    /// [`YpResponse::Chunk`]에 postcard `Vec<Vec<u8>>`(요청 기기의 `DeviceBundle` 인코딩들, 그 사용자의 다른 기기 것은 뺌). 묶음은 기기마다 봉인돼 있고
    /// 받는 쪽이 갱신의 `bundle_root` 증명으로 확인하므로 누가 내주든 같습니다. 답하는 쪽은 요청 기기가 활성 멤버
    /// 기기일 때만, 요청 기기마다 속도를 제한해 답합니다.
    GetKeyShard {
        space: SpaceId,
        blob: BlobId,
    },
    /// 기기 공개키(상자 키·서명 키) 묶어 받기(최대 [`MAX_DEVICE_KEYS_REQUEST`]개). 답은 [`YpResponse::DeviceKeys`]. 받는
    /// 쪽은 상태의 키 약속과 맞거나 체크포인트 기기 트리의 증명이 붙은 것만 받습니다.
    GetDeviceKeys {
        space: SpaceId,
        devices: Vec<DeviceId>,
    },
    /// 체크포인트 사슬: `from_seq`부터(포함) 최신까지 최대 [`crate::checkpoint::MAX_CHAIN_PER_REPLY`]개와 최신 체크포인트
    /// 이벤트·스냅숏 매니페스트. `from_seq`가 최신보다 크면 최신 (seq, ID)만. 답은 [`YpResponse::Checkpoint`].
    GetCheckpoint {
        space: SpaceId,
        from_seq: u64,
    },
    /// 키 그룹 사슬의 (순번, 뿌리 약속) 갱신 뒤 멤버 목록(체크포인트 키 머리에서 사슬을 세워 바닥 멤버를 모르는 기기).
    /// 답은 요청한 기기에게 봉인한 목록([`YpResponse::SealedKeys`]). 답하는 쪽은 요청 기기가 활성 멤버 기기이고 그 그룹
    /// 채널을 지금 볼 수 있을 때만 답하고, 받는 쪽은 체크포인트 키 머리의 멤버 요약과 맞는 목록만 씁니다.
    GetGroupMembers {
        space: SpaceId,
        group: KeyGroupId,
        seq: u64,
        root_commit: [u8; 32],
    },
    /// 휘발성 세션 키 감싸기(RAM 전용): 회전 공지(`VolatileRotation`)의 세대 키를 받는 기기 하나에게 봉인한 것. 서명은 없고
    /// 받는 쪽은 공지의 약속값과 맞는 키만 씁니다. `recipients`는 부모 세대 받는 기기 목록을 모르는 기기(가입자, 처음
    /// 세대)에게만 싣습니다.
    VolatileKeyWrap {
        space: SpaceId,
        channel: ChannelId,
        generation: u64,
        #[serde(with = "serde_bytes")]
        wrap: Vec<u8>,
        recipients: Option<Vec<DeviceId>>,
    },
    /// 채널 끝 묻기(채널 토픽을 켠 노드가 따라가지 않는 채널의 새 글이 있는지 알려고, 1분마다 멤버 한 곳에). 답은
    /// [`YpResponse::ChannelTips`]: 물은 채널 중 요청자가 기록을 볼 수 있는 모두 보는 일반 채널의 가장 늦은 메시지 논리
    /// 시계. 채널은 [`MAX_TIP_CHANNELS`]개까지.
    GetChannelTips {
        space: SpaceId,
        channels: Vec<ChannelId>,
    },
    /// 멤버 소개: 같은 공간 멤버 기기의 노드를 그 공간의 다른 멤버에게 묻습니다(위치 기록을 받아 둘 DHT 서버가 없는
    /// Tor 기기끼리). 답은 [`YpResponse::DeviceNode`]. 답하는 쪽은 요청 기기와 물은 기기가 모두 그 공간의 활성 멤버
    /// 기기일 때만 아는 노드와 링크용 주소(onion·릴레이 경유, 평문 IP 없음)를 줍니다. 답에는 서명이 없어 받는 쪽은
    /// 인증으로 그 기기임을 확인한 뒤에만 씁니다.
    FindDevice {
        space: SpaceId,
        user: UserId,
        device: DeviceId,
    },
    /// 멤버 노드 목록: 큰 크루에서 gossip 이웃 후보가 모자란 기기가 연결된 멤버에게, 그 멤버가 아는 같은 공간 멤버
    /// 기기의 노드를 묻습니다(누가 닿는지 모르니 `FindDevice`로는 고를 수 없음). 답은 [`YpResponse::MemberNodes`]: 7일
    /// 안에 연결에 성공한 기기를 섞어 [`MAX_MEMBER_NODES`]개까지, 지금 꺼진 기기도 담음. 검사·주소 규칙과 확인은
    /// `FindDevice`와 같음.
    GetMemberNodes {
        space: SpaceId,
    },
    /// 놓친 가입 확인(인증 전에도 받음): 상대가 내 `Auth`를 거절했지만 내 상태가 그 기기를 멤버로 담은 크루를 지목해,
    /// 그 기기가 꺼진 동안 들어온 나의 가입을 내게서 받아 가게 합니다. 받는 쪽은 지목된 크루 중 자기가 멤버인 것만
    /// 내게서 받고 다시 인증합니다(다른 크루는 묻지 않음). 크루는 [`MAX_CHECK_JOIN_SPACES`]개까지. 답은 `Ok`, 그런
    /// 크루가 없으면 `NotMember`.
    CheckJoin {
        spaces: Vec<SpaceId>,
        auth: Box<PeerAuth>,
    },
    /// 후원자 확인 결과([`SupporterShare`]): GitHub·Patreon 계정을 연결해 직접 확인한 내 기기가 인증된 내 다른 기기에게.
    /// 받는 쪽은 `valid_until`까지만 믿고 다시 나누지 않습니다. 답은 `Ok`.
    SupporterShare(SupporterShare),
    /// 보낸 기기의 표시 상태([`PresenceShare`], 저장하지 않음). 인증 직후와 바뀔 때 인증된 친구·멤버 기기에게. 답은 `Ok`.
    Presence(PresenceShare),
    /// 도전과제 뱃지([`BadgeMsg`]). 답은 `Ok`.
    Badges(Box<BadgeMsg>),
    /// 새 기기가 내 기기가 띄운 기기 연결 링크로 보내는 요청(인증 전에도 받음). 답은 [`YpResponse::DeviceLinkOpened`].
    DeviceLink(Box<DeviceLinkRequest>),
    /// 승인을 기다리는 새 기기의 물음. 답은 기다리면 `DeviceLinkOpened`, 승인이면 [`YpResponse::DeviceLinkGrant`],
    /// 거절이면 `Forbidden`, 링크가 끝났으면 `Expired`.
    DeviceLinkPoll {
        token: LinkToken,
    },
    /// 멤버의 서명 프로필·뱃지 카드 묶어 받기(최대 [`MAX_MEMBER_CARDS_REQUEST`]명). 답은 [`YpResponse::MemberCards`].
    /// 체크포인트로 들어와 경계 아래 이벤트를 받지 못한 기기가 멤버 기기에게 묻습니다.
    GetMemberCards {
        space: SpaceId,
        users: Vec<UserId>,
    },
}

impl YpRequest {
    /// 받는 쪽이 보낸 노드를 인증하기 전에도 받는 요청인지. 이런 요청과 그 응답은 압축하지 않고 보냅니다
    /// (인증 전 상대의 입력이 압축 해제기에 닿지 않게). 데몬 `inbound`의 인증 전 목록과 같아야 합니다.
    pub fn before_auth(&self) -> bool {
        match self {
            YpRequest::Ping
            | YpRequest::Auth(_)
            | YpRequest::RedeemFriendLink(_)
            | YpRequest::FriendAccepted { .. }
            | YpRequest::RedeemInvite(_)
            | YpRequest::FileLink(_)
            | YpRequest::CheckJoin { .. }
            | YpRequest::DeviceLink(_)
            | YpRequest::DeviceLinkPoll { .. } => true,
            YpRequest::Archive(a) => matches!(
                **a,
                crate::archive::ArchiveRequest::Designate(_)
                    | crate::archive::ArchiveRequest::Release { .. }
            ),
            _ => false,
        }
    }
}

/// [`YpRequest::DeviceLink`]. `entry`는 새 기기가 만든 기기 키로, 승인되면 이대로 내 기기 목록에 들어갑니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceLinkRequest {
    pub token: LinkToken,
    pub entry: DeviceEntry,
}

/// 승인된 새 기기에게 그 기기의 상자 키로 봉인해 주는 것. 신원 시드는 이렇게만 기기 밖으로 나갑니다.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceLinkGrant {
    pub seed: Secret32,
    /// 새 기기를 활성으로 담아 승인한 기기가 공동 서명한 내 기기 목록.
    pub devices: SignedDeviceList,
    pub profile: SignedProfile,
}

impl std::fmt::Debug for DeviceLinkGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceLinkGrant").finish_non_exhaustive()
    }
}

/// 두 기기 화면에 띄워 비교하는 6자리 번호. `nonce`는 승인할 기기가 요청을 받은 뒤에야 알려 주므로, QR을 훔쳐 본 사람이
/// 키를 바꿔 가며 같은 번호를 미리 맞춰 둘 수 없습니다.
pub fn device_link_code(token: &LinkToken, nonce: &[u8; 16], entry: &DeviceEntry) -> String {
    let mut h = blake3::Hasher::new_derive_key("yangpa/v3/device-link-code");
    h.update(&token.0);
    h.update(nonce);
    h.update(entry.device.as_bytes());
    h.update(&entry.key_commit());
    let n = u64::from_le_bytes(h.finalize().as_bytes()[..8].try_into().expect("8바이트"));
    format!("{:06}", n % 1_000_000)
}

/// [`YpRequest::Badges`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BadgeMsg {
    /// 보낸 사용자의 뱃지 카드. 인증 직후와 바뀔 때 인증된 친구·멤버·내 기기에게.
    Card(SignedBadgeCard),
    /// 개발자 계정이 받는 사용자에게 준 원년멤버 증서(개발자의 기기 → 그 사용자의 기기).
    Grant(FounderGrant),
    /// 내 기기끼리만: 보낸 기기가 센 값([`OwnBadgeCounts`]).
    Own(OwnBadgeCounts),
}

/// 뱃지 계산에 쓰는 기기별 값. 내 기기들이 시딩 바이트·통화 시간은 더하고 후원액은 가장 큰 것을 씁니다.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnBadgeCounts {
    /// 이 기기가 남(내 기기 제외)에게 내준 블롭 조각 바이트 합.
    pub seeded_bytes: u64,
    /// 이 기기가 후원자 확인에서 본 가장 큰 한 번 결제액(센트).
    pub support_cents: u64,
    /// 이 기기가 다른 사람과 함께한 통화 시간(초). 옛 빌드·옛 저장 값에는 없음.
    #[serde(default)]
    pub voice_secs: u64,
    /// 그 통화에서 내 화면을 공유한 시간(초).
    #[serde(default)]
    pub screen_secs: u64,
}

/// 기기의 표시 상태. 숨기(`Invisible`)는 연결·동기화는 그대로 하고 받는 쪽 화면에서만 오프라인으로 보이게 합니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PresenceStatus {
    Online,
    Away,
    Invisible,
}

/// [`YpRequest::Presence`]. `hide_last_seen`이면 받는 쪽이 나와의 마지막 동기화 시각을 화면에 보이지 않습니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceShare {
    pub status: PresenceStatus,
    pub hide_last_seen: bool,
}

/// 내 기기가 직접 확인한 후원자 확인 결과. 끝났으면(연결 해제·구독 끝) `active`가 거짓입니다.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupporterShare {
    /// 확인 경로(`github`·`patreon`).
    pub provider: String,
    /// 그 서비스의 계정 이름(화면 표시용).
    pub login: String,
    pub active: bool,
    pub valid_until: Timestamp,
}

impl std::fmt::Debug for SupporterShare {
    // 계정 이름은 로그에 남기지 않습니다.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupporterShare")
            .field("provider", &self.provider)
            .field("active", &self.active)
            .finish_non_exhaustive()
    }
}

/// [`YpRequest::CheckJoin`] 한 번에 지목하는 크루 수 상한.
pub const MAX_CHECK_JOIN_SPACES: usize = 16;

/// [`YpResponse::MemberNodes`] 한 번에 주는 노드 수 상한.
pub const MAX_MEMBER_NODES: usize = 16;

/// [`YpResponse::MemberNodes`]의 항목: 기기와 그 노드, 링크용 주소([`MAX_CONTACT_ADDRS`]개까지).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberNode {
    pub user: UserId,
    pub device: DeviceId,
    #[serde(with = "serde_bytes")]
    pub peer_id: Vec<u8>,
    pub addrs: Vec<String>,
}

/// [`YpRequest::GetChannelTips`] 한 번에 묻는 채널 수 상한(크루 채널 상한과 같음).
pub const MAX_TIP_CHANNELS: usize = 500;

/// [`YpRequest::GetChannelRoots`] 한 번에 묻는 뿌리 ID 상한.
pub const MAX_ROOT_IDS: usize = 512;

/// [`YpRequest::GetStateKeys`] 한 번에 묻는 상태 키 ID 상한.
pub const MAX_STATE_KEY_IDS: usize = 512;

/// 봉인한 키 그룹 표. 표 암호문은 무작위 키로 암호화한 블롭(`manifest`)이고 그 키만 받는 기기들에게 봉인돼
/// 있습니다(`envelope`). `sig`는 보낸 확정 기기가 (블롭 ID ‖ 봉투 해시)에 한 서명입니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyTableHandover {
    pub space: SpaceId,
    #[serde(with = "serde_bytes")]
    pub envelope: Vec<u8>,
    pub manifest: BlobManifest,
    #[serde(with = "serde_bytes")]
    pub ciphertext: Vec<u8>,
    pub sig: Sig,
    /// 표와 함께 두는 공개 메타데이터(마지막 전체 재키 시각, 채널 → 그룹·기록 공개 지도). 비밀이 아니라 봉인하지 않고,
    /// 받는 쪽은 인증된 연결로 표 보유 기기에게서 온 것만 씁니다. 없으면 새 확정 기기가 주간 전체 재키를 곧바로 하고
    /// 그사이 바뀐 채널을 놓칠 수 있습니다.
    #[serde(with = "serde_bytes")]
    pub meta: Vec<u8>,
}

/// [`YpRequest::GetMemberCards`] 하나에 물을 수 있는 사람 수.
pub const MAX_MEMBER_CARDS_REQUEST: usize = 128;
/// [`YpResponse::MemberCards`] 항목 하나의 상한(바이트). 데몬이 정하는 프로필·카드는 이보다 훨씬 작습니다. 부풀린 항목이
/// 다른 사람의 답을 밀어내지 않고, 한 묶음이 언제나 [`MAX_WIRE_MESSAGE`] 안에 듭니다.
pub const MAX_MEMBER_CARD_BYTES: usize = 32 * 1024;

/// [`YpResponse::MemberCards`]의 항목. 둘 다 신원 키 서명이라 답한 쪽이 지어낼 수 없습니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberCard {
    pub profile: Option<SignedProfile>,
    pub card: Option<SignedBadgeCard>,
}

/// [`YpRequest::GetDeviceKeys`] 하나에 물을 수 있는 기기 수.
pub const MAX_DEVICE_KEYS_REQUEST: usize = 512;

/// [`YpResponse::DeviceKeys`]의 항목: 키와, 답한 쪽이 붙일 수 있으면 체크포인트 기기 트리의 잎 증명.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceKeysItem {
    pub keys: DeviceKeys,
    pub proof: Option<DeviceLeafProof>,
}

/// [`YpRequest::GetCheckpoint`]의 답.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointReply {
    /// 답한 쪽의 최신 체크포인트(없으면 `None`).
    pub head: Option<CheckpointPin>,
    /// 요청한 순번부터의 사슬(평문 본문, 스스로 검증됨).
    pub chain: Vec<SignedCheckpoint>,
    /// 최신 체크포인트 이벤트(상태 키로 암호화된 그대로). 가입자는 받은 상태 키로 풀어 사슬의 마지막 본문과 같은지 봅니다.
    pub event: Option<SignedEvent>,
    /// 최신 스냅숏 블롭의 매니페스트(조각은 `BlobChunk`로).
    pub manifest: Option<BlobManifest>,
}

/// 이 기기의 P2P 쪽 노드(Tor 노드와 다른 임시 키)와 참여 중인 P2P 전용 채널, 그리고 받는 기기를 신뢰하는지.
///
/// - `peer_id`·`channels`: 직접 스웜 노드. 받는 기기를 신뢰하거나, 받는 기기가 멤버이고 볼 수 있는 내 참여 채널이 있을
///   때만 싣습니다(채널은 그런 것만).
/// - `addrs`: 직접 스웜 주소(IP가 드러남). 받는 기기를 신뢰하거나, 받는 기기가 실린 채널 중 하나에 함께 참여한다고
///   알려 왔을 때만.
/// - `exit_peer`: 받는 기기만 이 기기를 신뢰할 때, 이 기기가 Tor 출구를 거쳐 받는 기기의 직접 스웜에 걸 노드(주소 없음).
///
/// 모두 비면 참여·신뢰를 그만뒀다는 뜻입니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct P2pHello {
    /// 직접 스웜의 PeerId.
    #[serde(with = "serde_bytes")]
    pub peer_id: Vec<u8>,
    /// 직접 스웜 주소(최대 [`MAX_P2P_ADDRS`]개).
    pub addrs: Vec<String>,
    /// 참여 중인 (공간, 채널)(최대 [`MAX_P2P_CHANNELS`]개).
    pub channels: Vec<(SpaceId, ChannelId)>,
    /// 이 기기가 받는 기기를 신뢰함(신뢰 스위치: 받는 기기에게 IP를 보여도 됨).
    #[serde(default)]
    pub trusted: bool,
    /// 출구 스웜의 PeerId(빈 값이면 없음).
    #[serde(default, with = "serde_bytes")]
    pub exit_peer: Vec<u8>,
    /// 보낸 기기가 안내마다 키우는 값(밀리초 시각 기반). 받는 쪽은 그 기기에서 이미 받은 것보다 작으면 버립니다(늦게
    /// 닿은 옛 답이 새 안내를 덮지 않게). 0은 순서를 모르는 예전 빌드.
    #[serde(default)]
    pub seq: u64,
}

/// [`P2pHello::addrs`] 상한.
pub const MAX_P2P_ADDRS: usize = 16;

/// [`P2pHello::channels`] 상한.
pub const MAX_P2P_CHANNELS: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireErrorCode {
    Unauthenticated,
    NotMember,
    Forbidden,
    HistoryPrivate,
    NotSeeded,
    NotFound,
    InvalidToken,
    Expired,
    BadRequest,
    Internal,
}

// 응답은 매번 새로 만들어 바로 직렬화하므로 변형 크기 차이를 줄이려고 Box를 두지 않습니다.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum YpResponse {
    Ok,
    Error {
        code: WireErrorCode,
        message: String,
    },
    /// 대역폭 여유가 없어 대기열에 넣음. 이 시간 뒤 다시 요청.
    Busy {
        retry_after_ms: u32,
    },
    Auth(PeerAuth),
    /// 인증 거절: 제시한 기기가 내가 아는 그 사용자의 목록에서 폐기됨. 그 목록(신원 키·공동 서명)을 함께 보내, 그
    /// 기기가 스스로 확인하고 로컬 데이터를 지우게 합니다.
    Revoked {
        devices: SignedDeviceList,
    },
    /// 친구 요청 접수(상대 수락 대기).
    FriendPending,
    /// 초대 가입 접수: 확정 기기가 가입 기기를 넣은 크루 키 갱신을 낼 때까지 대기.
    InviteAccepted {
        space: SpaceId,
        events: Vec<SignedEvent>,
    },
    Heads {
        heads: Vec<EventId>,
        max_lamport: u64,
    },
    Fingerprints(Vec<RangeFingerprint>),
    Ids(Vec<EventId>),
    Events(Vec<SignedEvent>),
    Manifest(BlobManifest),
    Have {
        #[serde(with = "serde_bytes")]
        bitmap: Vec<u8>,
        total: u32,
    },
    Chunk {
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    /// 늦은 입장자에게 주는 휘발성 메시지와 남은 수명.
    VolatileEvents(Vec<VolatileItem>),
    /// `Social` 요청의 답.
    Social(SocialReply),
    Pong,
    /// `P2pHello`의 답: 받은 쪽의 안내.
    P2pHello(Box<P2pHello>),
    /// `P2pPunch`의 답: 받은 쪽의 막 고친 직접 스웜 노드·주소.
    P2pPunch(Box<P2pHello>),
    /// `GetChannelRoots`·`GetGroupKeys`·`GetStateKeys`·`GetGroupMembers`의 답: 요청한 기기의 상자 키로 봉인한 키(멤버
    /// 목록).
    SealedKeys(#[serde(with = "serde_bytes")] Vec<u8>),
    /// `GetDeviceKeys`의 답(아는 것만).
    DeviceKeys(Vec<DeviceKeysItem>),
    /// `GetCheckpoint`의 답.
    Checkpoint(Box<CheckpointReply>),
    /// `GetChannelTips`의 답: (채널, 가장 늦은 메시지의 논리 시계). 답하는 쪽이 가진 메시지가 없는 채널은 뺍니다.
    ChannelTips(Vec<(ChannelId, u64)>),
    /// `FindDevice`의 답: 그 기기의 노드와 링크용 주소([`MAX_CONTACT_ADDRS`]개까지).
    DeviceNode {
        #[serde(with = "serde_bytes")]
        peer_id: Vec<u8>,
        addrs: Vec<String>,
    },
    /// `GetMemberNodes`의 답.
    MemberNodes(Vec<MemberNode>),
    /// 기기 연결 요청을 받아 승인을 기다림. 번호는 이 값까지 넣어 셉니다([`device_link_code`]).
    DeviceLinkOpened {
        nonce: [u8; 16],
    },
    /// 기기 연결 승인: 요청한 기기의 상자 키로 봉인한 [`DeviceLinkGrant`].
    DeviceLinkGrant(#[serde(with = "serde_bytes")] Vec<u8>),
    /// `GetMemberCards`의 답(아는 것만).
    MemberCards(Vec<MemberCard>),
}

/// [`SocialRequest`]의 답. 비어 있는 칸은 줄 것이 없다는 뜻입니다.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SocialReply {
    /// `Devices`: 그 사용자의 아는 가장 새 기기 목록.
    #[serde(default)]
    pub devices: Option<SignedDeviceList>,
    /// `Devices`: 함께 있는 공간과 그 시작 이벤트(같은 사용자의 기기면 내가 멤버인 모든 공간).
    #[serde(default)]
    pub spaces: Vec<(SpaceId, EventId)>,
    /// `Devices`(내 기기에게만): 답한 기기의 연락처 기록.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contacts: Vec<OwnContact>,
    /// `Devices`(내 기기에게만): 답한 기기가 끊어서 지운 1:1 공간.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cut: Vec<SpaceId>,
    /// `Devices`(내 기기에게만): 답한 기기가 휴대폰인지.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mobile: bool,
    /// 답한 쪽의 서명 프로필.
    #[serde(default)]
    pub profile: Option<SignedProfile>,
    /// `Notes`: 보낸 기기에게 봉인한 답한 쪽의 메모 묶음(비면 없음).
    #[serde(default, with = "serde_bytes")]
    pub notes: Vec<u8>,
    /// `DmSpace`: 받은 공간 대신 답한 쪽이 계속
    /// 쓰는 1:1 공간.
    #[serde(default)]
    pub friend_space: Option<(SpaceId, EventId)>,
    /// `Contact`: 답한 노드의 링크용 주소.
    #[serde(default)]
    pub addrs: Vec<String>,
    /// `Contact`(친구·내 기기에게만): 답한 기기의 노드 카드(지금 빌드는 하나, 옛 빌드는 모드마다 하나).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<NodeContact>,
    /// `Propose`: 이제 서로 친구인지(아니면 답한 쪽이 요청을 받아 둠).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub friends: bool,
}

/// 휘발성 스냅숏 항목. 받는 쪽은 `remaining_ms`가 지나면(받은 시점 기준) 지웁니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolatileItem {
    pub event: SignedEvent,
    pub remaining_ms: Option<u64>,
}

impl YpResponse {
    pub fn error(code: WireErrorCode, message: impl Into<String>) -> Self {
        YpResponse::Error {
            code,
            message: message.into(),
        }
    }
}

/// gossip 토픽: 크루 전체(상태·크루 키 갱신·모두 보는 채널) 또는 크루 안의 비공개 키 그룹(그 그룹 채널, 기기가 많은
/// 그룹만). 채널 토픽을 켠 노드는 모두 보는 채널 이벤트를 채널 묶음 토픽에, 멘션·답장이 있는 것은 알림 토픽에도 싣습니다.
/// 토픽마다 허용 피어 목록이 따로 있습니다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GossipTopic {
    Crew(SpaceId),
    Group(SpaceId, KeyGroupId),
    /// 채널 묶음(`channel_bucket`, `0..CHANNEL_BUCKETS`).
    Bucket(SpaceId, u8),
    /// 멘션·답장이 있는 모두 보는 채널 메시지.
    Notify(SpaceId),
}

/// 크루마다 채널 묶음 토픽 수.
pub const CHANNEL_BUCKETS: u8 = 16;

/// 채널이 실리는 묶음. 공간마다 섞여 같은 채널 ID가 공간을 넘어 같은 묶음으로 드러나지 않습니다.
pub fn channel_bucket(space: &SpaceId, channel: &ChannelId) -> u8 {
    let mut h = blake3::Hasher::new_derive_key("yangpa/v2/channel-bucket");
    h.update(space.as_bytes());
    h.update(channel.as_bytes());
    h.finalize().as_bytes()[0] % CHANNEL_BUCKETS
}

impl GossipTopic {
    pub fn space(&self) -> SpaceId {
        match self {
            GossipTopic::Crew(s)
            | GossipTopic::Group(s, _)
            | GossipTopic::Bucket(s, _)
            | GossipTopic::Notify(s) => *s,
        }
    }

    /// 토픽 이름 `yp2/<16바이트 hex>`. 공간·그룹 ID를 드러내지 않게 해시하고, 크루와 그룹 토픽이 이름으로 갈리지 않게
    /// 같은 모양입니다(앞의 `2`는 형식 판: 예전 판 노드와 메시가 섞이지 않게).
    pub fn name(&self) -> String {
        let mut h = blake3::Hasher::new();
        match self {
            GossipTopic::Crew(s) => {
                h.update(b"yangpa/v2/topic/crew\0");
                h.update(s.as_bytes());
            }
            GossipTopic::Group(s, g) => {
                h.update(b"yangpa/v2/topic/group\0");
                h.update(s.as_bytes());
                h.update(g.as_bytes());
            }
            GossipTopic::Bucket(s, b) => {
                h.update(b"yangpa/v2/topic/bucket\0");
                h.update(s.as_bytes());
                h.update(&[*b]);
            }
            GossipTopic::Notify(s) => {
                h.update(b"yangpa/v2/topic/notify\0");
                h.update(s.as_bytes());
            }
        }
        format!("yp2/{}", hex::encode(&h.finalize().as_bytes()[..16]))
    }
}

/// 크루 gossip 토픽 이름([`GossipTopic::name`]).
pub fn gossip_topic(space: &SpaceId) -> String {
    GossipTopic::Crew(*space).name()
}

/// gossip으로 실리는 메시지.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GossipMessage {
    Events {
        space: SpaceId,
        events: Vec<SignedEvent>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{DeviceEntry, DeviceList};

    #[test]
    fn device_link_code_is_six_digits_and_covers_every_input() {
        let entry = |key: &SigningKeypair| DeviceEntry {
            device: DeviceId(key.public()),
            box_key: crate::device::BoxPublicKey::filled(0),
            sign_keys: crate::device::DeviceSignKeys::of(key),
            name: "새 기기".into(),
            added_at: Timestamp(1),
            revoked_at: None,
        };
        let a = entry(&SigningKeypair::generate());
        let b = entry(&SigningKeypair::generate());
        let (t1, t2) = (LinkToken([1; 16]), LinkToken([2; 16]));
        let code = device_link_code(&t1, &[1; 16], &a);
        assert_eq!(code.len(), 6);
        assert!(code.bytes().all(|c| c.is_ascii_digit()));
        assert_eq!(code, device_link_code(&t1, &[1; 16], &a));
        // 번호가 우연히 겹칠 확률은 10⁻⁶이라 세 입력을 모두 바꿔 봐도 시험이 흔들리지 않습니다.
        assert_ne!(code, device_link_code(&t2, &[1; 16], &a));
        assert_ne!(code, device_link_code(&t1, &[2; 16], &a));
        assert_ne!(code, device_link_code(&t1, &[1; 16], &b));
    }

    #[test]
    fn peer_auth_bound_to_connection() {
        let id = SigningKeypair::generate();
        let dev = SigningKeypair::generate();
        let list = DeviceList {
            user: UserId(id.public()),
            version: 1,
            devices: vec![DeviceEntry {
                device: DeviceId(dev.public()),
                box_key: crate::device::BoxPublicKey::filled(0),
                sign_keys: crate::device::DeviceSignKeys::of(&dev),
                name: "pc".into(),
                added_at: Timestamp(0),
                revoked_at: None,
            }],
            otp: None,
        }
        .sign(&id)
        .unwrap();
        let auth = PeerAuth::create(&dev, list, b"me", b"you");
        auth.verify(b"me", b"you", 60_000).unwrap();
        assert!(auth.verify(b"you", b"me", 60_000).is_err());
        assert!(auth.verify(b"me", b"other", 60_000).is_err());
    }

    #[test]
    fn friend_accepted_profile_is_optional() {
        // 프로필 필드가 없던 형식도 읽혀야 합니다(자기 기술 형식에서 기본값 None).
        let req = YpRequest::FriendAccepted {
            token: LinkToken([1; 16]),
            space: SpaceId([2; 16]),
            events: Vec::new(),
            profile: None,
            contact: None,
        };
        let mut v = serde_json::to_value(&req).unwrap();
        v["FriendAccepted"]
            .as_object_mut()
            .unwrap()
            .remove("profile");
        let back: YpRequest = serde_json::from_value(v).unwrap();
        assert_eq!(back, req);

        let id = SigningKeypair::generate();
        let profile = crate::device::Profile {
            version: 1,
            name: "세영".into(),
            ..Default::default()
        }
        .sign(&id)
        .unwrap();
        let req = YpRequest::FriendAccepted {
            token: LinkToken([1; 16]),
            space: SpaceId([2; 16]),
            events: Vec::new(),
            profile: Some(Box::new(profile)),
            contact: Some(ContactTerms {
                expires_at: Some(Timestamp(5)),
            }),
        };
        let back: YpRequest = serde_json::from_value(serde_json::to_value(&req).unwrap()).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn social_messages_roundtrip() {
        let req = YpRequest::Social(SocialRequest::Notes {
            sealed: vec![1, 2, 3],
        });
        let back: YpRequest = serde_json::from_value(serde_json::to_value(&req).unwrap()).unwrap();
        assert_eq!(back, req);
        let req = YpRequest::Social(SocialRequest::Contact {
            addrs: vec!["xxxx.onion:4001".into()],
            nodes: vec![NodeContact {
                peer_id: vec![1, 2],
                onion: Some("yyyy.onion:4001".into()),
            }],
        });
        let back: YpRequest = serde_json::from_value(serde_json::to_value(&req).unwrap()).unwrap();
        assert_eq!(back, req);
        // 노드 칸이 없는 옛 형식도 읽힘.
        let old = serde_json::json!({ "Social": { "Contact": { "addrs": ["a"] } } });
        assert_eq!(
            serde_json::from_value::<YpRequest>(old).unwrap(),
            YpRequest::Social(SocialRequest::Contact {
                addrs: vec!["a".into()],
                nodes: Vec::new(),
            })
        );
        // 빈 답은 칸을 모두 빼도 읽힘(새 칸을 더해도 이전 형식과 호환).
        let empty: SocialReply = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, SocialReply::default());
        let reply = YpResponse::Social(SocialReply {
            spaces: vec![(SpaceId([1; 16]), EventId([2; 32]))],
            notes: vec![9],
            ..Default::default()
        });
        let back: YpResponse =
            serde_json::from_value(serde_json::to_value(&reply).unwrap()).unwrap();
        assert_eq!(back, reply);
    }

    #[test]
    fn moderation_roundtrip() {
        for req in [
            YpRequest::Moderation(ModerationRequest::Report {
                space: SpaceId([1; 16]),
                sealed: vec![1, 2, 3],
            }),
            YpRequest::Moderation(ModerationRequest::Resolved {
                space: SpaceId([2; 16]),
                sealed: vec![4],
            }),
        ] {
            let back: YpRequest =
                serde_json::from_value(serde_json::to_value(&req).unwrap()).unwrap();
            assert_eq!(back, req);
        }
    }

    #[test]
    fn topic_hides_space_id() {
        let s = SpaceId::random();
        let t = gossip_topic(&s);
        assert!(!t.contains(&s.to_string()));
        assert_eq!(t, gossip_topic(&s));
        assert!(t.starts_with("yp2/") && t.len() == 4 + 32);
        // 그룹 토픽은 같은 모양이지만 그룹마다·크루와 다릅니다.
        let g = |b: u8| GossipTopic::Group(s, KeyGroupId([b; 16])).name();
        assert_eq!(g(1).len(), t.len());
        assert_ne!(g(1), t);
        assert_ne!(g(1), g(2));
        assert_eq!(GossipTopic::Group(s, KeyGroupId([1; 16])).space(), s);
        let names: std::collections::HashSet<String> = (0..CHANNEL_BUCKETS)
            .map(|b| GossipTopic::Bucket(s, b).name())
            .chain([GossipTopic::Notify(s).name(), t.clone()])
            .collect();
        assert_eq!(names.len(), CHANNEL_BUCKETS as usize + 2);
        let c = ChannelId::random();
        assert!(channel_bucket(&s, &c) < CHANNEL_BUCKETS);
        assert_eq!(channel_bucket(&s, &c), channel_bucket(&s, &c));
    }

    #[test]
    fn signal_join_defaults_and_media_key_roundtrip() {
        // 상태 필드가 없던 참가 알림도 읽혀야 합니다.
        let old: SignalPayload = serde_json::from_str(r#"{"type":"join","muted":true}"#).unwrap();
        assert_eq!(
            old,
            SignalPayload::Join {
                muted: true,
                deafened: false,
                screen: false,
                generation: 0,
                reply: false,
                sfu: None,
                session: 0,
            }
        );
        let key = SignalPayload::MediaKey {
            generation: 3,
            sealed: vec![1, 2, 3],
        };
        let v = serde_json::to_value(&key).unwrap();
        assert_eq!(v["type"], "media_key");
        assert_eq!(serde_json::from_value::<SignalPayload>(v).unwrap(), key);
    }

    #[test]
    fn sfu_payloads_roundtrip_and_shapes() {
        let caps = SfuCaps {
            preferred: true,
            public: true,
            relayed: 0,
            rtt_ms: Some(20),
            capacity: 32,
        };
        let join = SignalPayload::Join {
            muted: false,
            deafened: false,
            screen: false,
            generation: 1,
            reply: false,
            sfu: Some(caps),
            session: 0,
        };
        let v = serde_json::to_value(&join).unwrap();
        assert_eq!(v["sfu"]["capacity"], 32);
        assert_eq!(serde_json::from_value::<SignalPayload>(v).unwrap(), join);
        // 앱이 쓰는 모양(snake_case 태그, 트랙 표의 출처 기기는 생략 가능).
        let offer: SignalPayload = serde_json::from_str(
            r#"{"type":"sfu_offer","session":7,"sdp":"v=0","tracks":[{"mid":"0","source":"audio"}]}"#,
        )
        .unwrap();
        assert_eq!(
            offer,
            SignalPayload::SfuOffer {
                session: 7,
                sdp: "v=0".into(),
                tracks: vec![SfuTrack {
                    mid: "0".into(),
                    device: None,
                    source: TrackSource::Audio
                }],
            }
        );
        let select: SignalPayload = serde_json::from_value(serde_json::json!({
            "type": "sfu_select",
            "device": DeviceId([1; 32]),
            "source": "screen",
            "layer": "low",
        }))
        .unwrap();
        assert!(matches!(
            select,
            SignalPayload::SfuSelect {
                source: TrackSource::Screen,
                layer: SfuLayer::Low,
                ..
            }
        ));
        for p in [
            SignalPayload::SfuPlan {
                seq: 3,
                host: Some(DeviceId([2; 32])),
            },
            SignalPayload::SfuHost { caps: None },
            SignalPayload::SfuClose { session: 1 },
        ] {
            let v = serde_json::to_value(&p).unwrap();
            assert_eq!(serde_json::from_value::<SignalPayload>(v).unwrap(), p);
        }
    }
}
