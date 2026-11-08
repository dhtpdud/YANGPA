//! 테스트용 이벤트 생성 도우미.

use yangpa_core::body::MessageBody;
use yangpa_core::event::{EVENT_VERSION, EventHeader, EventKind, KeyRef, SignedEvent};
use yangpa_core::ids::{ChannelId, ChannelRootId, DeviceId, EventId, SpaceId, StateKeyId, UserId};
use yangpa_core::model::SecurityLevel;
use yangpa_core::sign::SigningKeypair;
use yangpa_core::time::Timestamp;

use crate::events::PersistentEvent;
use crate::messages::{MessageContent, MessageKind, MessageRecord};

pub(crate) struct Fx {
    pub dev: SigningKeypair,
    pub user: SigningKeypair,
    pub space: SpaceId,
    pub channel: ChannelId,
}

impl Fx {
    pub fn new() -> Self {
        Fx {
            dev: SigningKeypair::generate(),
            user: SigningKeypair::generate(),
            space: SpaceId::random(),
            channel: ChannelId::random(),
        }
    }

    pub fn user_id(&self) -> UserId {
        UserId(self.user.public())
    }

    pub fn header(
        &self,
        kind: EventKind,
        channel: Option<ChannelId>,
        parents: Vec<EventId>,
        lamport: u64,
    ) -> EventHeader {
        // 메시지 레인은 기록 키, 상태 레인·키 갱신은 상태 키(형식 3판 `check_shape`).
        let key = match kind {
            _ if kind.lane() == yangpa_core::event::Lane::Message => KeyRef::Record {
                root: ChannelRootId::random(),
            },
            _ => KeyRef::State(StateKeyId::random()),
        };
        EventHeader {
            v: EVENT_VERSION,
            space: self.space,
            channel,
            kind,
            author: DeviceId(self.dev.public()),
            user: UserId(self.user.public()),
            parents,
            lamport,
            created_at: Timestamp(1_700_000_000_000 + lamport),
            key,
            body_hash: [0; 32],
            body_len: 0,
            scope: None,
            state_anchor: Vec::new(),
        }
    }

    /// 채널 메시지 이벤트(본문은 테스트용 바이트: 실제로는 기록 키 암호문).
    pub fn msg(&self, parents: Vec<EventId>, lamport: u64, body: &[u8]) -> SignedEvent {
        self.msg_in(self.channel, parents, lamport, body)
    }

    pub fn msg_in(
        &self,
        channel: ChannelId,
        parents: Vec<EventId>,
        lamport: u64,
        body: &[u8],
    ) -> SignedEvent {
        SignedEvent::sign(
            self.header(EventKind::Message, Some(channel), parents, lamport),
            body.to_vec(),
            &self.dev,
        )
        .expect("서명")
    }

    pub fn kind_event(&self, kind: EventKind, parents: Vec<EventId>, lamport: u64) -> SignedEvent {
        let channel = kind.requires_channel().then_some(self.channel);
        SignedEvent::sign(
            self.header(kind, channel, parents, lamport),
            vec![lamport as u8; 40],
            &self.dev,
        )
        .expect("서명")
    }

    pub fn volatile_msg(&self, generation: u64, lamport: u64, body: &[u8]) -> SignedEvent {
        let mut h = self.header(EventKind::Message, Some(self.channel), vec![], lamport);
        h.key = KeyRef::Volatile { generation };
        SignedEvent::sign(h, body.to_vec(), &self.dev).expect("서명")
    }
}

pub(crate) fn persist(e: &SignedEvent) -> PersistentEvent {
    PersistentEvent::new(e.clone(), SecurityLevel::Normal).expect("디스크 등급")
}

pub(crate) fn view(e: &SignedEvent, text: &str) -> MessageRecord {
    let content = MessageContent {
        body: MessageBody {
            text: text.into(),
            ..Default::default()
        },
        forum: None,
    };
    MessageRecord::from_header(e.id(), &e.header, MessageKind::Message, content).expect("보기")
}
