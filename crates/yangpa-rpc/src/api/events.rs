//! 이벤트 알림. 서버가 화면 프레임(약 16ms) 단위로 묶어
//! `{"jsonrpc":"2.0","method":"events","params":{"batch":[Notification,...]}}`로 보냅니다.

use serde::{Deserialize, Serialize};
use yangpa_core::device::Profile;
use yangpa_core::ids::{BlobId, CallId, ChannelId, DeviceId, EventId, SpaceId, UserId};
use yangpa_core::wire::SignalPayload;

use super::Scope;
use super::params::DeviceLinkRequestView;
use super::serde_util::SecretString;
use super::types::{
    BridgesView, CheckpointAlertKind, DeviceLinkStatus, FlairView, MessageView, NetStatus,
    PendingConfirmation, PresenceView, Topology, TransferRates, VoiceParticipant,
};
use crate::server::AuthInfo;

/// 알림 묶음을 싣는 JSON-RPC 알림 메서드 이름.
pub const EVENTS_METHOD: &str = "events";

/// OS 알림에 쓸 제목·본문. 휘발성 채널은 본문이 없습니다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyView {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// 보낸 사람(알림에 프사를 띄울 때 씀). 휘발성 채널은 보낸 사람을 알리지 않아 없습니다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<NotifySender>,
}

/// [`NotifyView::sender`]: 프사(없으면 이름 머리글자로 그리는 기본 프사)를 그릴 재료.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifySender {
    pub user: UserId,
    /// 표시 이름(프로필 이름, 없으면 ID 앞부분).
    pub name: String,
    /// 프로필 이미지 블롭. 알림이 가리키는 공간으로 `blob.read`(키 출처는 비워 둠)해 읽습니다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<BlobId>,
}

/// 알림 하나. `type` 필드로 구분합니다(docs/api.md 이벤트 표).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Notification {
    #[serde(rename = "message.new")]
    MessageNew {
        space: SpaceId,
        channel: ChannelId,
        message: MessageView,
        notify: NotifyView,
    },
    /// 수정·반응·삭제.
    #[serde(rename = "message.updated")]
    MessageUpdated {
        space: SpaceId,
        channel: ChannelId,
        message: MessageView,
    },
    #[serde(rename = "message.expired")]
    MessageExpired {
        space: SpaceId,
        channel: ChannelId,
        ids: Vec<EventId>,
    },
    /// 상태 변경(채널·역할·멤버).
    #[serde(rename = "space.updated")]
    SpaceUpdated { space: SpaceId },
    #[serde(rename = "friend.request")]
    FriendRequest {
        request_id: String,
        user: UserId,
        profile: Profile,
    },
    #[serde(rename = "friend.accepted")]
    FriendAccepted { user: UserId, space: SpaceId },
    /// 연락처 종류가 바뀜(임시 연락처가 만료되거나 친구가 됨). `friend.list`를 다시 읽으세요.
    #[serde(rename = "friend.updated")]
    FriendUpdated { user: UserId },
    /// 화면에 보일 접속 상태가 바뀜(`MemberView`의 `online`·`away`·`last_seen`과 같은 뜻, 나 자신도).
    #[serde(rename = "presence")]
    Presence {
        user: UserId,
        online: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        away: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_seen: Option<u64>,
    },
    /// 이 기기의 표시 상태가 바뀜(`presence.set`, 기간 끝남, 입력 없음).
    #[serde(rename = "presence.self")]
    PresenceSelf(PresenceView),
    /// 사용자(나 자신도)의 후원자 표시가 바뀜.
    #[serde(rename = "flair")]
    Flair(FlairView),
    /// 받은 친구·멤버(또는 내 다른 기기)의 서명 프로필이 더 새 판으로 바뀜. `friend.list`·`space.get`을 다시 읽으세요.
    #[serde(rename = "profile.updated")]
    ProfileUpdated { user: UserId },
    /// 그 사용자(나 자신도)의 뱃지 카드가 바뀜. `badge.get`을 다시 읽으세요.
    #[serde(rename = "badges.updated")]
    BadgesUpdated { user: UserId },
    #[serde(rename = "voice.signal")]
    VoiceSignal {
        space: SpaceId,
        channel: ChannelId,
        call: CallId,
        from: DeviceId,
        payload: SignalPayload,
    },
    /// Tor 통화(`Topology::Tor`)의 소리 패킷. `data`는 보낸 앱이 봉인한 바이트(base64)이고, 보낸 기기(`from`)를 프레임
    /// AAD로 검사해 엽니다.
    #[serde(rename = "voice.media")]
    VoiceMedia {
        space: SpaceId,
        channel: ChannelId,
        call: CallId,
        from: DeviceId,
        data: String,
    },
    #[serde(rename = "voice.participants")]
    VoiceParticipants {
        space: SpaceId,
        channel: ChannelId,
        participants: Vec<VoiceParticipant>,
        generation: u64,
        media_key: SecretString,
        topology: Topology,
        /// SFU 토폴로지일 때 호스트 기기(메시이거나 넘겨받는 중이면 없음).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sfu_host: Option<DeviceId>,
        /// 그 호스트 기기의 사용자(통화 밖 호스트 노드도 누구의 노드인지 보여 줌).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sfu_host_user: Option<UserId>,
    },
    #[serde(rename = "blob.progress")]
    BlobProgress {
        space: SpaceId,
        blob: BlobId,
        have: u32,
        total: u32,
    },
    #[serde(rename = "net.status")]
    NetStatus(NetStatus),
    /// 브리지 설정이 바뀜(`net.set_bridges`, 또는 검열을 만나 데몬이 Snowflake를 켬: `auto`).
    #[serde(rename = "net.bridges")]
    Bridges(BridgesView),
    /// 전송 속도(1초마다, 바뀌었을 때만. 모두 0이 되면 한 번 보내고 멈춤).
    #[serde(rename = "transfer.rates")]
    TransferRates(TransferRates),
    #[serde(rename = "confirm.pending")]
    ConfirmPending(PendingConfirmation),
    #[serde(rename = "seed.coverage_warning")]
    SeedCoverageWarning { space: SpaceId, channel: ChannelId },
    /// 새 신고(운영진 노드에만). 내용은 `report.list`로 읽습니다.
    #[serde(rename = "report.new")]
    ReportNew { space: SpaceId, report_id: String },
    /// 이 연결이 느려 알림 `count`개를 버렸음(docs/api.md 보충). 받으면 필요한 상태를 다시 읽으세요.
    #[serde(rename = "events.dropped")]
    EventsDropped { count: u64 },
    /// 띄운 기기 연결 링크로 새 기기가 승인을 청함(`device.link.decide`). 같은 링크를 다른 기기가 또 쓰면 `contested`로 다시 옵니다.
    #[serde(rename = "device.link_request")]
    DeviceLinkRequest(DeviceLinkRequestView),
    /// 이 기기의 연결 요청이 끝남. `approved`면 신원이 설치됐고 첫 동기화가 돕니다.
    #[serde(rename = "device.link_result")]
    DeviceLinkResult { status: DeviceLinkStatus },
    /// 내 기기로 연결된 뒤 첫 동기화가 끝남(`identity.get`의 `first_syncing`이 꺼짐).
    #[serde(rename = "device.synced")]
    DeviceSynced,
    /// 후원 확인 상태가 바뀌었거나 로그인이 끝났음. `supporter.status`로 다시 읽습니다. 로그인이 실패했으면 `error`.
    #[serde(rename = "supporter.updated")]
    SupporterUpdated {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// 이 기기가 다른 기기에서 폐기되어 로컬 데이터를 모두 지웠음(신원도 없어짐).
    #[serde(rename = "device.wiped")]
    DeviceWiped,
    /// 체크포인트 경보: 같은 순번의 갈라진 체크포인트를 봤거나, 체크포인트로 들어온 상태를 버리고 처음부터 따라잡음.
    #[serde(rename = "checkpoint.alert")]
    CheckpointAlert {
        space: SpaceId,
        kind: CheckpointAlertKind,
    },
    /// 누군가 그 채널에서 글을 쓰는 중(`channel.typing`, 받은 뒤 몇 초 동안만 보여 줌). 익명 채널이면 `user`는
    /// [`ANONYMOUS_USER`](super::ANONYMOUS_USER)이고 `anon_name`에 그 채널의 가명이 있습니다.
    #[serde(rename = "channel.typing")]
    Typing {
        space: SpaceId,
        channel: ChannelId,
        user: UserId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        anon_name: Option<String>,
    },
}

impl Notification {
    /// 모든 알림 종류(구독 주제 검사용).
    pub const TYPES: &'static [&'static str] = &[
        "message.new",
        "message.updated",
        "message.expired",
        "space.updated",
        "friend.request",
        "friend.accepted",
        "friend.updated",
        "presence",
        "presence.self",
        "flair",
        "profile.updated",
        "badges.updated",
        "voice.signal",
        "voice.media",
        "voice.participants",
        "blob.progress",
        "net.status",
        "net.bridges",
        "transfer.rates",
        "confirm.pending",
        "seed.coverage_warning",
        "report.new",
        "events.dropped",
        "device.link_request",
        "device.link_result",
        "device.synced",
        "supporter.updated",
        "device.wiped",
        "channel.typing",
        "checkpoint.alert",
    ];

    /// `type` 값.
    pub fn kind(&self) -> &'static str {
        match self {
            Notification::MessageNew { .. } => "message.new",
            Notification::MessageUpdated { .. } => "message.updated",
            Notification::MessageExpired { .. } => "message.expired",
            Notification::SpaceUpdated { .. } => "space.updated",
            Notification::FriendRequest { .. } => "friend.request",
            Notification::FriendAccepted { .. } => "friend.accepted",
            Notification::FriendUpdated { .. } => "friend.updated",
            Notification::Presence { .. } => "presence",
            Notification::PresenceSelf(_) => "presence.self",
            Notification::Flair(_) => "flair",
            Notification::ProfileUpdated { .. } => "profile.updated",
            Notification::BadgesUpdated { .. } => "badges.updated",
            Notification::Typing { .. } => "channel.typing",
            Notification::VoiceSignal { .. } => "voice.signal",
            Notification::VoiceMedia { .. } => "voice.media",
            Notification::VoiceParticipants { .. } => "voice.participants",
            Notification::BlobProgress { .. } => "blob.progress",
            Notification::NetStatus(_) => "net.status",
            Notification::Bridges(_) => "net.bridges",
            Notification::TransferRates(_) => "transfer.rates",
            Notification::ConfirmPending(_) => "confirm.pending",
            Notification::SeedCoverageWarning { .. } => "seed.coverage_warning",
            Notification::ReportNew { .. } => "report.new",
            Notification::EventsDropped { .. } => "events.dropped",
            Notification::DeviceLinkRequest(_) => "device.link_request",
            Notification::DeviceLinkResult { .. } => "device.link_result",
            Notification::DeviceSynced => "device.synced",
            Notification::SupporterUpdated { .. } => "supporter.updated",
            Notification::DeviceWiped => "device.wiped",
            Notification::CheckpointAlert { .. } => "checkpoint.alert",
        }
    }

    /// 알림이 속한 공간(없으면 전역 알림).
    pub fn space(&self) -> Option<SpaceId> {
        match self {
            Notification::MessageNew { space, .. }
            | Notification::MessageUpdated { space, .. }
            | Notification::MessageExpired { space, .. }
            | Notification::SpaceUpdated { space }
            | Notification::FriendAccepted { space, .. }
            | Notification::VoiceSignal { space, .. }
            | Notification::VoiceMedia { space, .. }
            | Notification::VoiceParticipants { space, .. }
            | Notification::BlobProgress { space, .. }
            | Notification::SeedCoverageWarning { space, .. }
            | Notification::Typing { space, .. }
            | Notification::ReportNew { space, .. }
            | Notification::CheckpointAlert { space, .. } => Some(*space),
            Notification::ConfirmPending(p) => p.space,
            Notification::FriendRequest { .. }
            | Notification::FriendUpdated { .. }
            | Notification::Presence { .. }
            | Notification::PresenceSelf(_)
            | Notification::Flair(_)
            | Notification::ProfileUpdated { .. }
            | Notification::BadgesUpdated { .. }
            | Notification::NetStatus(_)
            | Notification::Bridges(_)
            | Notification::TransferRates(_)
            | Notification::EventsDropped { .. }
            | Notification::DeviceLinkRequest(_)
            | Notification::DeviceLinkResult { .. }
            | Notification::DeviceSynced
            | Notification::SupporterUpdated { .. }
            | Notification::DeviceWiped => None,
        }
    }
}

/// 구독 주제가 알림 종류와 맞는지. 주제가 비면 전부, `*`는 전부, `message`는 `message.*`.
pub fn topic_matches(topics: &[String], kind: &str) -> bool {
    if topics.is_empty() {
        return true;
    }
    topics.iter().any(|t| {
        let t = t.trim();
        t == "*"
            || t == kind
            || kind
                .strip_prefix(t)
                .is_some_and(|rest| rest.starts_with('.'))
    })
}

/// 주제 문자열이 알려진 알림 종류(또는 그 앞부분, `*`)인지.
pub fn valid_topic(topic: &str) -> bool {
    let t = topic.trim();
    t == "*"
        || Notification::TYPES
            .iter()
            .any(|k| topic_matches(&[t.to_owned()], k))
}

/// 기본 알림 걸러내기.
///
/// - 공간 제한 토큰: 허용 공간의 알림과 `events.dropped`만 보냅니다(친구 요청·접속 상태 같은 전역 알림 제외).
/// - 자동화 토큰: 휘발성 메시지는 내용을 지운 채로 보냅니다(채널별 API 읽기 허용 여부는 데몬만 알므로,
///   허용 채널을 살리려면 데몬이 `RpcHandler::filter_notification`을 바꿔 구현합니다).
/// - 통화: 프레임 키(`voice.participants`의 `media_key`)와 WebRTC 신호(`voice.signal`, 상대 IP가 든 ICE 후보·SDP)는
///   통화에 참가할 수 있는 사람 토큰(`write` 범위, 자동화 아님)에만 보냅니다. 나머지는 키를 지우고(`generation: 0`)
///   신호 알림은 보내지 않습니다.
///
/// `None`이면 보내지 않고, `Some`이면 그 알림(바뀌었을 수 있음)을 보냅니다.
pub fn default_notification_filter(auth: &AuthInfo, n: &Notification) -> Option<Notification> {
    if !auth.spaces.is_empty() {
        match n.space() {
            Some(s) if auth.spaces.contains(&s) => {}
            None if matches!(n, Notification::EventsDropped { .. }) => {}
            _ => return None,
        }
    }
    if auth.automation || !auth.has_scope(Scope::Write) {
        match n {
            Notification::VoiceSignal { .. } | Notification::VoiceMedia { .. } => return None,
            Notification::VoiceParticipants {
                space,
                channel,
                participants,
                topology,
                sfu_host,
                sfu_host_user,
                ..
            } => {
                return Some(Notification::VoiceParticipants {
                    space: *space,
                    channel: *channel,
                    participants: participants.clone(),
                    generation: 0,
                    media_key: SecretString::default(),
                    topology: *topology,
                    sfu_host: *sfu_host,
                    sfu_host_user: *sfu_host_user,
                });
            }
            _ => {}
        }
    }
    if auth.automation {
        match n {
            Notification::MessageNew {
                space,
                channel,
                message,
                notify,
            } if message.volatile => {
                return Some(Notification::MessageNew {
                    space: *space,
                    channel: *channel,
                    message: message.redacted(),
                    notify: NotifyView {
                        title: notify.title.clone(),
                        body: None,
                        sender: None,
                    },
                });
            }
            Notification::MessageUpdated {
                space,
                channel,
                message,
            } if message.volatile => {
                return Some(Notification::MessageUpdated {
                    space: *space,
                    channel: *channel,
                    message: message.redacted(),
                });
            }
            // 누가 무엇을 받는지는 사람에게만(전체 속도는 그대로).
            Notification::TransferRates(r) if !r.uploads.is_empty() => {
                return Some(Notification::TransferRates(TransferRates {
                    uploads: Vec::new(),
                    ..r.clone()
                }));
            }
            // 직접 넣은 브리지 줄은 공개되지 않은 브리지 주소라 사람에게만(`net.bridges`도 사람만).
            Notification::Bridges(_) => return None,
            _ => {}
        }
    }
    Some(n.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Scope;
    use serde_json::json;
    use yangpa_core::time::Timestamp;

    fn msg(volatile: bool) -> MessageView {
        MessageView {
            id: EventId([1; 32]),
            author: UserId([2; 32]),
            author_device: DeviceId([3; 32]),
            created_at: Timestamp(5),
            lamport: 1,
            text: "비밀 내용".into(),
            attachments: vec![],
            embeds: vec![],
            reply_to: None,
            reactions: vec![],
            edited_at: None,
            deleted: false,
            volatile,
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

    fn auth(spaces: Vec<SpaceId>, automation: bool) -> AuthInfo {
        AuthInfo {
            token_id: "t".into(),
            token_name: "test".into(),
            scopes: vec![Scope::Read],
            spaces,
            automation,
        }
    }

    #[test]
    fn notification_shapes() {
        let n = Notification::SpaceUpdated {
            space: SpaceId([7; 16]),
        };
        assert_eq!(
            serde_json::to_value(&n).unwrap(),
            json!({"type": "space.updated", "space": SpaceId([7; 16]).to_string()})
        );
        let n = Notification::Presence {
            user: UserId([1; 32]),
            online: true,
            away: false,
            last_seen: None,
        };
        let v = serde_json::to_value(&n).unwrap();
        assert_eq!(v["type"], "presence");
        // 옛 형식(자리 비움·마지막 동기화 없음) 그대로 내보내고 읽습니다.
        assert!(v.get("away").is_none() && v.get("last_seen").is_none());
        assert_eq!(serde_json::from_value::<Notification>(v).unwrap(), n);
        let n = Notification::PresenceSelf(PresenceView::default());
        let v = serde_json::to_value(&n).unwrap();
        assert_eq!(v["type"], "presence.self");
        assert_eq!(v["mode"], "online");
        assert_eq!(serde_json::from_value::<Notification>(v).unwrap(), n);

        let n = Notification::ProfileUpdated {
            user: UserId([1; 32]),
        };
        assert_eq!(
            serde_json::to_value(&n).unwrap(),
            json!({"type": "profile.updated", "user": UserId([1; 32]).to_string()})
        );
        assert_eq!(n.kind(), "profile.updated");
        assert!(Notification::TYPES.contains(&n.kind()));
        assert_eq!(n.space(), None);

        let n = Notification::NetStatus(NetStatus {
            peer_id: "p".into(),
            ..Default::default()
        });
        let v = serde_json::to_value(&n).unwrap();
        assert_eq!(v["type"], "net.status");
        assert_eq!(v["peer_id"], "p");
        assert_eq!(serde_json::from_value::<Notification>(v).unwrap(), n);

        let n = Notification::MessageNew {
            space: SpaceId([1; 16]),
            channel: ChannelId([2; 16]),
            message: msg(true),
            notify: NotifyView {
                title: "게임 모임 #비밀".into(),
                body: None,
                sender: None,
            },
        };
        let v = serde_json::to_value(&n).unwrap();
        assert_eq!(v["type"], "message.new");
        assert_eq!(v["notify"], json!({"title": "게임 모임 #비밀"}));
        assert_eq!(serde_json::from_value::<Notification>(v).unwrap(), n);

        let n = Notification::VoiceSignal {
            space: SpaceId([1; 16]),
            channel: ChannelId([2; 16]),
            call: CallId([3; 16]),
            from: DeviceId([4; 32]),
            payload: SignalPayload::Ice {
                candidate: "c".into(),
                sdp_mid: None,
                sdp_mline_index: Some(0),
            },
        };
        let v = serde_json::to_value(&n).unwrap();
        assert_eq!(v["type"], "voice.signal");
        assert_eq!(v["payload"]["type"], "ice");
        assert_eq!(serde_json::from_value::<Notification>(v).unwrap(), n);

        for t in Notification::TYPES {
            assert!(valid_topic(t));
        }
        assert_eq!(
            Notification::EventsDropped { count: 1 }.kind(),
            "events.dropped"
        );
    }

    #[test]
    fn transfer_rates_hide_downloaders_from_automation() {
        use crate::api::types::{TransferRates, UploadRate};
        let n = Notification::TransferRates(TransferRates {
            up_bps: 10,
            down_bps: 20,
            uploads: vec![UploadRate {
                space: SpaceId([1; 16]),
                channel: None,
                blob: BlobId([2; 32]),
                user: UserId([3; 32]),
                bps: 10,
            }],
        });
        let v = serde_json::to_value(&n).unwrap();
        assert_eq!(v["type"], "transfer.rates");
        assert_eq!(v["up_bps"], 10);
        assert_eq!(n.space(), None);
        let Some(Notification::TransferRates(r)) =
            default_notification_filter(&auth(vec![], true), &n)
        else {
            panic!("자동화 토큰에도 전체 속도는 감");
        };
        assert!(r.uploads.is_empty());
        assert_eq!((r.up_bps, r.down_bps), (10, 20));
        assert_eq!(
            default_notification_filter(&auth(vec![], false), &n),
            Some(n.clone())
        );
        // 공간 제한 토큰에는 전역 알림이 가지 않습니다.
        assert!(default_notification_filter(&auth(vec![SpaceId([1; 16])], false), &n).is_none());
    }

    #[test]
    fn topics() {
        assert!(topic_matches(&[], "presence"));
        assert!(topic_matches(&["message".into()], "message.new"));
        assert!(!topic_matches(&["message".into()], "messages.x"));
        assert!(!topic_matches(&["message.new".into()], "message.updated"));
        assert!(topic_matches(&["*".into()], "voice.signal"));
        assert!(valid_topic("voice"));
        assert!(!valid_topic("bogus"));
    }

    #[test]
    fn filter_space_restriction_and_volatile() {
        let allowed = SpaceId([1; 16]);
        let other = SpaceId([9; 16]);
        let restricted = auth(vec![allowed], false);
        assert!(
            default_notification_filter(
                &restricted,
                &Notification::SpaceUpdated { space: allowed }
            )
            .is_some()
        );
        assert!(
            default_notification_filter(&restricted, &Notification::SpaceUpdated { space: other })
                .is_none()
        );
        assert!(
            default_notification_filter(
                &restricted,
                &Notification::Presence {
                    user: UserId([1; 32]),
                    online: true,
                    away: false,
                    last_seen: None,
                }
            )
            .is_none()
        );
        // 프로필 변경은 공간에 묶이지 않아 공간 제한 토큰에는 가지 않습니다.
        assert!(
            default_notification_filter(
                &restricted,
                &Notification::ProfileUpdated {
                    user: UserId([1; 32])
                }
            )
            .is_none()
        );
        assert!(
            default_notification_filter(&restricted, &Notification::EventsDropped { count: 3 })
                .is_some()
        );

        let n = Notification::MessageNew {
            space: allowed,
            channel: ChannelId([2; 16]),
            message: msg(true),
            notify: NotifyView {
                title: "t".into(),
                body: Some("비밀 내용".into()),
                sender: None,
            },
        };
        let ai = auth(vec![], true);
        let Some(Notification::MessageNew {
            message, notify, ..
        }) = default_notification_filter(&ai, &n)
        else {
            panic!("메시지 알림이어야 함");
        };
        assert!(message.text.is_empty());
        assert!(notify.body.is_none());
        // 사람 토큰은 그대로.
        assert_eq!(
            default_notification_filter(&auth(vec![], false), &n),
            Some(n.clone())
        );
        // 일반 메시지는 자동화 토큰에도 그대로.
        let normal = Notification::MessageUpdated {
            space: allowed,
            channel: ChannelId([2; 16]),
            message: msg(false),
        };
        assert_eq!(
            default_notification_filter(&ai, &normal),
            Some(normal.clone())
        );
    }

    /// 통화 프레임 키·WebRTC 신호는 `write` 범위 사람 토큰에만 간다.
    #[test]
    fn filter_voice_keys_and_signals() {
        let space = SpaceId([1; 16]);
        let channel = ChannelId([2; 16]);
        let roster = Notification::VoiceParticipants {
            space,
            channel,
            participants: Vec::new(),
            generation: 7,
            media_key: SecretString::new("a2V5"),
            topology: Topology::Sfu,
            sfu_host: Some(DeviceId([5; 32])),
            sfu_host_user: Some(UserId([6; 32])),
        };
        let signal = Notification::VoiceSignal {
            space,
            channel,
            call: CallId([3; 16]),
            from: DeviceId([4; 32]),
            payload: SignalPayload::Ice {
                candidate: "candidate:1 1 udp 1 192.0.2.7 50000 typ host".into(),
                sdp_mid: None,
                sdp_mline_index: Some(0),
            },
        };
        let mut writer = auth(vec![], false);
        writer.scopes = vec![Scope::Read, Scope::Write];
        let mut ai_writer = auth(vec![], true);
        ai_writer.scopes = vec![Scope::Read, Scope::Write];
        let owner = AuthInfo {
            scopes: vec![Scope::Owner],
            ..auth(vec![], false)
        };
        for a in [&writer, &owner] {
            assert_eq!(
                default_notification_filter(a, &roster),
                Some(roster.clone())
            );
            assert_eq!(
                default_notification_filter(a, &signal),
                Some(signal.clone())
            );
        }
        for a in [&auth(vec![], false), &auth(vec![], true), &ai_writer] {
            let Some(Notification::VoiceParticipants {
                generation,
                media_key,
                sfu_host,
                sfu_host_user,
                ..
            }) = default_notification_filter(a, &roster)
            else {
                panic!("참가자 알림은 키 없이 감");
            };
            assert_eq!(generation, 0);
            assert_eq!(sfu_host, Some(DeviceId([5; 32])));
            assert_eq!(sfu_host_user, Some(UserId([6; 32])));
            assert!(media_key.expose().is_empty());
            assert!(default_notification_filter(a, &signal).is_none());
        }
    }
}
