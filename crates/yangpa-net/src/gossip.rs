//! GossipSub 설정과 닫힌 스웜(토픽마다 허용된 멤버 노드끼리만) 규칙.
//!
//! - 토픽 = [`GossipTopic`](크루, 또는 기기가 많은 비공개 키 그룹), 메시지 ID = BLAKE3(데이터). 같은 이벤트 묶음은 누가
//!   다시 올려도 한 번만 전달됩니다.
//! - 메시지에 libp2p 출처·서명을 싣지 않습니다(`Anonymous`). 이벤트마다 혼합 서명이 있어 출처 서명은 홉마다 약 120B와
//!   서명 확인만 더하고, 처음 올린 노드를 드러냅니다.
//! - 어느 공간의 허용 목록에도 없는 피어, 그리고 허용되지 않은 공간의 토픽을 구독한 피어는 애플리케이션
//!   점수를 크게 깎아 메시·전달·보내기 대상에서 뺍니다(그레이리스트). 토픽 이름은 구독 메시지로 연결된
//!   피어에게 보이므로, 이렇게 하지 않으면 다른 공간의 멤버가 그 토픽을 구독해 이벤트 헤더(작성자·채널·시각)를
//!   받아 갈 수 있습니다.
//!   gossipsub은 구독을 받는 순간 (점수를 바꾸기 전에) 메시에 넣을 수 있어 다음 하트비트까지 한 번은 샐 수
//!   있으므로, 한 번 이렇게 구독한 피어는 그 공간에 허용될 때까지 기억해 구독을 풀었다 다시 하는 식으로
//!   틈을 반복해 얻지 못하게 합니다.
//! - 공간별 허용 목록 밖의 피어가 전달한 메시지는 수동 검증 단계에서 거부합니다.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use libp2p::PeerId;
use libp2p::gossipsub::{
    self, IdentTopic, MaxCountSubscriptionFilter, MessageAcceptance, MessageId, PeerScoreParams,
    PeerScoreThresholds, TopicHash, TopicScoreParams, TopicSubscriptionFilter, ValidationMode,
};
use yangpa_core::ids::SpaceId;
use yangpa_core::wire::{GossipMessage, GossipTopic, MAX_EVENTS_PER_MESSAGE};

use crate::config::MAX_GOSSIP_MESSAGE;

/// 토픽 하나에 허용할 피어 수 상한(메모리 보호).
pub const MAX_ALLOWED_PER_SPACE: usize = 65_536;
/// 어떤 공간에도 속하지 않은 피어의 애플리케이션 점수. 그레이리스트 기준(-80)보다 훨씬 낮습니다.
pub const NON_MEMBER_SCORE: f64 = -1000.0;
/// 피어 하나가 구독할 수 있는 토픽 수. 넘치면 그 피어의 구독 메시지를 무시합니다(메모리 보호).
/// libp2p 기본값(100)은 공간이 많은 사용자를 끊어 버리므로 넉넉히 둡니다.
pub const MAX_PEER_TOPICS: usize = 1024;

/// 원격 피어의 구독을 받을지 거르는 필터: 이 앱의 토픽 형식(`yp2/<16바이트 hex>`)만 받습니다.
#[derive(Clone, Copy, Debug, Default)]
pub struct YpTopicFilter;

impl TopicSubscriptionFilter for YpTopicFilter {
    fn can_subscribe(&mut self, topic_hash: &TopicHash) -> bool {
        is_yp_topic(topic_hash.as_str())
    }
}

/// gossipsub에 넘기는 구독 필터(형식 검사 + 피어별 개수 상한).
pub type SubscriptionFilter = MaxCountSubscriptionFilter<YpTopicFilter>;

pub fn subscription_filter() -> SubscriptionFilter {
    MaxCountSubscriptionFilter {
        filter: YpTopicFilter,
        max_subscribed_topics: MAX_PEER_TOPICS,
        max_subscriptions_per_request: MAX_PEER_TOPICS,
    }
}

/// [`GossipTopic::name`] 형식인지.
fn is_yp_topic(t: &str) -> bool {
    t.strip_prefix("yp2/").is_some_and(|h| {
        h.len() == 32
            && h.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    })
}

/// gossipsub 토픽.
pub fn topic(t: &GossipTopic) -> IdentTopic {
    IdentTopic::new(t.name())
}

/// 메시지 ID = BLAKE3(데이터). 서명·순번이 달라도 내용이 같으면 같은 메시지로 봅니다.
pub fn message_id(msg: &gossipsub::Message) -> MessageId {
    MessageId::from(blake3::hash(&msg.data).as_bytes().to_vec())
}

/// 소규모 그룹에 맞춘 gossipsub 설정.
pub fn build_config() -> Result<gossipsub::Config, String> {
    let heartbeat = Duration::from_secs(1);
    // 프로토콜 ID는 기본값(meshsub 1.3·1.2부터)이라 IDONTWANT가 돕니다. 예전 판 노드와는 토픽 이름(`yp2/`)이 달라
    // 메시가 섞이지 않습니다.
    gossipsub::ConfigBuilder::default()
        .validation_mode(ValidationMode::Anonymous)
        .validate_messages()
        .message_id_fn(message_id)
        // 서명·토픽·프레이밍 여유.
        .max_transmit_size(MAX_GOSSIP_MESSAGE + 8 * 1024)
        .duplicate_cache_time(Duration::from_secs(120))
        .heartbeat_interval(heartbeat)
        .mesh_n(6)
        .mesh_n_low(4)
        .mesh_n_high(8)
        .mesh_outbound_min(2)
        .gossip_lazy(6)
        // 수동 검증 결과를 기다리는 동안 메시지가 캐시에서 빠지지 않게 넉넉히(하트비트 10번).
        .history_length(10)
        .history_gossip(3)
        // 첫 하트비트(허용되지 않은 구독자를 메시에서 빼는 시점)를 기본 5초보다 당깁니다.
        .heartbeat_initial_delay(heartbeat)
        // 작성자는 연결된 구독자 전부(최대 256)가 아니라 메시 피어(최대 mesh_n_high)에게만 올립니다. 메시가 비어
        // 있어도 구독 중인 연결 피어에서 mesh_n곳을 골라 보내고, 그런 피어가 하나도 없을 때만 `NoPeers`입니다
        // (libp2p-gossipsub 0.50 `filter_publish_candidates`, 데몬은 그때 직접 보냄).
        .flood_publish(false)
        .build()
        .map_err(|e| e.to_string())
}

/// 피어 점수: 애플리케이션 점수(멤버 여부)와 잘못된 메시지 감점만 씁니다. 같은 IP 감점은 릴레이·Tor 뒤의
/// 피어를 부당하게 깎으므로 끕니다.
pub fn score_params() -> (PeerScoreParams, PeerScoreThresholds) {
    let params = PeerScoreParams {
        app_specific_weight: 1.0,
        ip_colocation_factor_weight: 0.0,
        ..Default::default()
    };
    (params, PeerScoreThresholds::default())
}

/// 구독 토픽의 점수 설정. 조용한 채팅방에서 '전달이 적다'는 이유로 감점되지 않게 P1~P3을 끄고,
/// 거부된 메시지(P4)만 크게 감점합니다.
pub fn topic_score_params() -> TopicScoreParams {
    TopicScoreParams {
        topic_weight: 1.0,
        time_in_mesh_weight: 0.0,
        first_message_deliveries_weight: 0.0,
        mesh_message_deliveries_weight: 0.0,
        mesh_failure_penalty_weight: 0.0,
        invalid_message_deliveries_weight: -100.0,
        invalid_message_deliveries_decay: gossipsub::score_parameter_decay(Duration::from_secs(
            600,
        )),
        ..Default::default()
    }
}

/// 토픽별 허용 피어와 토픽 이름 대응.
#[derive(Default)]
pub struct GossipState {
    allowed: HashMap<GossipTopic, HashSet<PeerId>>,
    /// 피어가 몇 개 토픽의 허용 목록에 있는지.
    member_count: HashMap<PeerId, usize>,
    topics: HashMap<TopicHash, GossipTopic>,
    subscribed: HashSet<GossipTopic>,
    /// 허용되지 않은 크루 토픽을 구독한 적이 있는 피어와 그 토픽. 그 토픽에 허용되거나 어느 토픽의 멤버도 아니게
    /// 되면 지웁니다(멤버만 기록되므로 허용 목록 크기로 묶입니다). 그룹 토픽은 기억하지 않습니다: 같은 크루 멤버가
    /// 열람 권한을 잃은 직후에도 구독이 남아 있을 수 있어, 기억하면 그 멤버가 크루 토픽에서도 계속 빠집니다. 지금
    /// 구독 중인 동안은 그룹 토픽도 걸러 냅니다([`Self::score_for`]).
    intrusions: HashMap<PeerId, HashSet<GossipTopic>>,
}

impl GossipState {
    /// 허용 목록을 바꾸고, 멤버 여부가 바뀐 피어를 돌려줍니다.
    pub fn set_allowed(&mut self, t: GossipTopic, peers: Vec<PeerId>) -> Vec<PeerId> {
        let new: HashSet<PeerId> = peers.into_iter().take(MAX_ALLOWED_PER_SPACE).collect();
        let old = self.allowed.remove(&t).unwrap_or_default();
        let mut changed = Vec::new();
        for p in old.difference(&new) {
            if let Some(c) = self.member_count.get_mut(p) {
                *c -= 1;
                if *c == 0 {
                    self.member_count.remove(p);
                    self.intrusions.remove(p);
                    changed.push(*p);
                }
            }
        }
        for p in &new {
            if let Some(ts) = self.intrusions.get_mut(p) {
                ts.remove(&t);
                if ts.is_empty() {
                    self.intrusions.remove(p);
                }
            }
        }
        for p in new.difference(&old) {
            let c = self.member_count.entry(*p).or_insert(0);
            *c += 1;
            if *c == 1 {
                changed.push(*p);
            }
        }
        self.topics.insert(topic(&t).hash(), t);
        if new.is_empty() {
            if !self.subscribed.contains(&t) {
                self.topics.remove(&topic(&t).hash());
            }
        } else {
            self.allowed.insert(t, new);
        }
        changed
    }

    /// 어느 토픽이든 허용 목록에 있는지.
    pub fn is_member(&self, peer: &PeerId) -> bool {
        self.member_count.contains_key(peer)
    }

    /// 피어의 애플리케이션 점수를 계산합니다. `peer_topics`는 그 피어가 지금 구독한 토픽(gossipsub이 아는 것).
    ///
    /// 어느 토픽에도 없거나, 허용 목록이 정해진 토픽을 허용되지 않은 채 구독했으면(지금, 크루 토픽은 전에도)
    /// 비멤버 점수입니다. 그 토픽의 허용 목록에 들어가면 바로 풀립니다.
    pub fn score_for<'a>(
        &mut self,
        peer: &PeerId,
        peer_topics: impl IntoIterator<Item = &'a TopicHash>,
    ) -> f64 {
        if !self.is_member(peer) {
            return NON_MEMBER_SCORE;
        }
        let intruded: Vec<GossipTopic> = peer_topics
            .into_iter()
            .filter_map(|t| self.topic_of(t))
            .filter(|t| self.allowed.get(t).is_some_and(|set| !set.contains(peer)))
            .collect();
        if !intruded.is_empty() {
            let crew: Vec<GossipTopic> = intruded
                .into_iter()
                .filter(|t| matches!(t, GossipTopic::Crew(_)))
                .collect();
            if !crew.is_empty() {
                self.intrusions.entry(*peer).or_default().extend(crew);
            }
            return NON_MEMBER_SCORE;
        }
        let remembered = self.intrusions.get(peer).is_some_and(|ts| {
            ts.iter()
                .any(|s| self.allowed.get(s).is_some_and(|set| !set.contains(peer)))
        });
        if remembered { NON_MEMBER_SCORE } else { 0.0 }
    }

    #[cfg(test)]
    pub fn is_allowed(&self, t: &GossipTopic, peer: &PeerId) -> bool {
        self.allowed.get(t).is_some_and(|s| s.contains(peer))
    }

    /// 이 토픽의 허용 목록이 정해져 있는지.
    pub fn has_allowed(&self, t: &GossipTopic) -> bool {
        self.allowed.contains_key(t)
    }

    /// 구독 상태를 기록합니다. 새로 구독했으면 true.
    pub fn subscribe(&mut self, t: GossipTopic) -> bool {
        self.topics.insert(topic(&t).hash(), t);
        self.subscribed.insert(t)
    }

    /// 구독 해제를 기록합니다. 구독 중이었으면 true.
    pub fn unsubscribe(&mut self, t: &GossipTopic) -> bool {
        let was = self.subscribed.remove(t);
        if !self.allowed.contains_key(t) {
            self.topics.remove(&topic(t).hash());
        }
        was
    }

    #[cfg(test)]
    pub fn is_subscribed(&self, t: &GossipTopic) -> bool {
        self.subscribed.contains(t)
    }

    pub fn topic_of(&self, topic: &TopicHash) -> Option<GossipTopic> {
        self.topics.get(topic).copied()
    }

    /// 수동 검증의 네트워크 단계: 토픽·허용 목록·형식 검사. 통과하면 해석한 메시지를 돌려주고, 아니면
    /// gossipsub에 알릴 판정을 돌려줍니다. 서명·멤버십·권한 같은 내용 검사는 데몬이 합니다.
    pub fn check(
        &self,
        propagation_source: &PeerId,
        message: &gossipsub::Message,
    ) -> Result<(GossipTopic, GossipMessage), MessageAcceptance> {
        let Some(topic) = self.topic_of(&message.topic) else {
            return Err(MessageAcceptance::Ignore);
        };
        if !self.subscribed.contains(&topic) {
            return Err(MessageAcceptance::Ignore);
        }
        // 허용 목록이 아직 없으면 판단할 수 없으므로 감점 없이 버립니다.
        let Some(allowed) = self.allowed.get(&topic) else {
            return Err(MessageAcceptance::Ignore);
        };
        // 출처를 싣지 않습니다(`Anonymous`). gossipsub이 먼저 거르지만 같은 규칙을 여기서도 둡니다.
        if message.source.is_some() {
            return Err(MessageAcceptance::Reject);
        }
        // 전달한 피어가 멤버가 아니면 위반입니다.
        if !allowed.contains(propagation_source) {
            return Err(MessageAcceptance::Reject);
        }
        // 처음 올린 노드는 알 수 없고(출처 없음) 알 필요도 없습니다. 허용 목록은 이 노드가 만나(인증해) 본 멤버 기기의
        // 노드뿐이라, 기기가 8개를 넘는 크루에서는 만나지 않은 멤버가 쓴 메시지가 대부분입니다. 전달한 멤버가 검증해
        // 넘긴 것이고 서명·멤버십·권한은 데몬이 다시 봅니다. 어느 상태에서도 틀린 것이면
        // 전달한 멤버가 감점되고, 지금 상태로 갈린 거부는 감점하지 않습니다(데몬 `sync_service::gossip_verdict`).
        // 압축 표시 바이트 1바이트만큼 여유를 둡니다(줄지 않으면 그대로 실림). 풀린 크기도 같은 한도.
        if message.data.len() > MAX_GOSSIP_MESSAGE + 1 {
            return Err(MessageAcceptance::Reject);
        }
        let plain = yangpa_zl::unpack(&message.data, MAX_GOSSIP_MESSAGE)
            .map_err(|_| MessageAcceptance::Reject)?;
        let msg: GossipMessage =
            postcard::from_bytes(&plain).map_err(|_| MessageAcceptance::Reject)?;
        match &msg {
            GossipMessage::Events { space: s, events } => {
                if *s != topic.space() || events.is_empty() || events.len() > MAX_EVENTS_PER_MESSAGE
                {
                    return Err(MessageAcceptance::Reject);
                }
            }
        }
        Ok((topic, msg))
    }
}

/// 보낼 gossip 메시지를 직렬화하고 검사합니다.
pub fn encode_outgoing(space: &SpaceId, msg: &GossipMessage) -> Result<Vec<u8>, crate::NetError> {
    match msg {
        GossipMessage::Events { space: s, events } => {
            if s != space {
                return Err(crate::NetError::InvalidInput(
                    "메시지의 공간과 토픽 공간이 다름".into(),
                ));
            }
            if events.is_empty() || events.len() > MAX_EVENTS_PER_MESSAGE {
                return Err(crate::NetError::InvalidInput(
                    "이벤트 개수가 범위를 벗어남".into(),
                ));
            }
        }
    }
    let data =
        postcard::to_stdvec(msg).map_err(|e| crate::NetError::InvalidInput(e.to_string()))?;
    if data.len() > MAX_GOSSIP_MESSAGE {
        return Err(crate::NetError::TooLarge {
            size: data.len(),
            max: MAX_GOSSIP_MESSAGE,
        });
    }
    Ok(yangpa_zl::pack(&data).to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: SpaceId) -> GossipTopic {
        GossipTopic::Crew(s)
    }

    fn msg(space: &SpaceId, source: Option<PeerId>, data: Vec<u8>) -> gossipsub::Message {
        gossipsub::Message {
            source,
            data,
            sequence_number: Some(1),
            topic: topic(&c(*space)).hash(),
        }
    }

    fn sample_events(space: SpaceId) -> GossipMessage {
        use yangpa_core::event::{EventHeader, EventKind, KeyRef, SignedEvent};
        use yangpa_core::ids::{ChannelId, ChannelRootId, DeviceId, UserId};
        use yangpa_core::sign::SigningKeypair;
        let dev = SigningKeypair::generate();
        let header = EventHeader {
            v: yangpa_core::event::EVENT_VERSION,
            space,
            channel: Some(ChannelId::random()),
            kind: EventKind::Message,
            author: DeviceId(dev.public()),
            user: UserId([1; 32]),
            parents: vec![],
            lamport: 1,
            created_at: yangpa_core::Timestamp(1),
            key: KeyRef::Record {
                root: ChannelRootId::random(),
            },
            body_hash: [0; 32],
            body_len: 0,
            scope: None,
            state_anchor: Vec::new(),
        };
        let ev = SignedEvent::sign(header, vec![1, 2, 3], &dev).unwrap();
        GossipMessage::Events {
            space,
            events: vec![ev],
        }
    }

    #[test]
    fn config_builds() {
        let c = build_config().unwrap();
        assert!(
            !c.flood_publish(),
            "작성자는 연결된 구독자 전부가 아니라 메시 피어에게만 올림"
        );
        let (p, t) = score_params();
        p.validate().unwrap();
        t.validate().unwrap();
        topic_score_params().validate().unwrap();
        assert!(NON_MEMBER_SCORE * p.app_specific_weight < t.graylist_threshold);
    }

    #[test]
    fn message_id_is_content_hash() {
        let s = SpaceId::random();
        let a = msg(&s, Some(PeerId::random()), b"same".to_vec());
        let mut b = msg(&s, Some(PeerId::random()), b"same".to_vec());
        b.sequence_number = Some(99);
        assert_eq!(message_id(&a), message_id(&b));
        assert_ne!(
            message_id(&a),
            message_id(&msg(&s, None, b"other".to_vec()))
        );
    }

    #[test]
    fn membership_counts() {
        let mut g = GossipState::default();
        let (s1, s2) = (SpaceId::random(), SpaceId::random());
        let (a, b) = (PeerId::random(), PeerId::random());
        let changed = g.set_allowed(c(s1), vec![a, b]);
        assert_eq!(changed.len(), 2);
        assert!(g.is_member(&a) && g.is_member(&b));
        // a는 두 공간의 멤버: s1에서 빠져도 여전히 멤버.
        assert!(g.set_allowed(c(s2), vec![a]).is_empty());
        let changed = g.set_allowed(c(s1), vec![]);
        assert_eq!(changed, vec![b]);
        assert!(g.is_member(&a));
        assert!(!g.is_member(&b));
        assert_eq!(g.score_for(&b, []), NON_MEMBER_SCORE);
        assert_eq!(g.score_for(&a, []), 0.0);
        assert!(g.is_allowed(&c(s2), &a));
        assert!(!g.is_allowed(&c(s1), &a));
    }

    #[test]
    fn subscribing_to_a_foreign_space_topic_greylists() {
        let mut g = GossipState::default();
        let (s1, s2, unknown) = (SpaceId::random(), SpaceId::random(), SpaceId::random());
        let (a, b) = (PeerId::random(), PeerId::random());
        g.set_allowed(c(s1), vec![a, b]);
        g.set_allowed(c(s2), vec![a]);
        g.subscribe(c(s1));
        g.subscribe(c(s2));
        let (t1, t2, tu) = (
            topic(&c(s1)).hash(),
            topic(&c(s2)).hash(),
            topic(&c(unknown)).hash(),
        );
        // a는 두 공간 모두 멤버, b는 s1만 멤버.
        assert_eq!(g.score_for(&a, [&t1, &t2]), 0.0);
        assert_eq!(g.score_for(&b, [&t1]), 0.0);
        // 이 노드가 모르는 공간의 토픽은 상관하지 않는다.
        assert_eq!(g.score_for(&b, [&t1, &tu]), 0.0);
        assert!(!g.intrusions.contains_key(&b));
        // b가 s2 토픽을 구독하면(멤버가 아닌데) 메시·전달 대상에서 빠진다.
        assert_eq!(g.score_for(&b, [&t1, &t2]), NON_MEMBER_SCORE);
        // 구독을 풀었다 다시 해서 메시에 들어갈 틈을 반복해 얻지 못하게, 허용되기 전까지는 기억한다.
        assert_eq!(g.score_for(&b, [&t1]), NON_MEMBER_SCORE);
        // 허용 목록에 넣으면 바로 풀린다.
        g.set_allowed(c(s2), vec![a, b]);
        assert_eq!(g.score_for(&b, [&t1, &t2]), 0.0);
        assert_eq!(g.score_for(&b, [&t1]), 0.0);
        // 다시 빠진 뒤 구독하면 다시 걸리고, 어느 공간의 멤버도 아니게 되면 기록도 지운다.
        g.set_allowed(c(s2), vec![a]);
        assert_eq!(g.score_for(&b, [&t2]), NON_MEMBER_SCORE);
        assert!(g.intrusions.contains_key(&b));
        g.set_allowed(c(s1), vec![a]);
        assert!(!g.intrusions.contains_key(&b));
        g.set_allowed(c(s1), vec![a, b]);
        assert_eq!(g.score_for(&b, [&t1]), 0.0);
        // 허용 목록이 아직 없는 공간(구독만 함)은 판단하지 않는다.
        let s3 = SpaceId::random();
        g.subscribe(c(s3));
        assert!(!g.has_allowed(&c(s3)));
        assert_eq!(g.score_for(&b, [&topic(&c(s3)).hash()]), 0.0);
    }

    #[test]
    fn group_topic_intrusion_is_not_remembered() {
        let mut g = GossipState::default();
        let s = SpaceId::random();
        let grp = GossipTopic::Group(s, yangpa_core::ids::KeyGroupId([7; 16]));
        let (a, b) = (PeerId::random(), PeerId::random());
        g.set_allowed(c(s), vec![a, b]);
        g.set_allowed(grp, vec![a]);
        g.subscribe(c(s));
        g.subscribe(grp);
        let (tc, tg) = (topic(&c(s)).hash(), topic(&grp).hash());
        assert_eq!(g.score_for(&a, [&tc, &tg]), 0.0);
        // 그룹에 없는 크루 멤버가 그룹 토픽을 구독한 동안은 걸러 냅니다.
        assert_eq!(g.score_for(&b, [&tc, &tg]), NON_MEMBER_SCORE);
        // 구독을 풀면(열람 권한을 잃은 직후 등) 크루 토픽에서는 바로 다시 받습니다.
        assert_eq!(g.score_for(&b, [&tc]), 0.0);
        assert!(!g.intrusions.contains_key(&b));
    }

    #[test]
    fn topic_filter_accepts_only_app_topics() {
        let mut f = subscription_filter();
        assert!(f.can_subscribe(&topic(&c(SpaceId::random())).hash()));
        for bad in [
            "yp/",
            "yp/xyz",
            "/ipfs/kad",
            "yp2/0123456789abcdef0123456789ABCDEF",
            "yp2/0123456789abcdef0123456789abcdef0",
            // 예전 판 토픽.
            "yp/0123456789abcdef0123456789abcdef",
        ] {
            assert!(!f.can_subscribe(&TopicHash::from_raw(bad)), "{bad}");
        }
    }

    #[test]
    fn check_enforces_closed_swarm() {
        let mut g = GossipState::default();
        let space = SpaceId::random();
        let (member, member2, outsider) = (PeerId::random(), PeerId::random(), PeerId::random());
        g.set_allowed(c(space), vec![member, member2]);
        let data = encode_outgoing(&space, &sample_events(space)).unwrap();

        // 구독하지 않은 토픽은 무시.
        assert_eq!(
            g.check(&member, &msg(&space, None, data.clone()))
                .unwrap_err(),
            MessageAcceptance::Ignore
        );
        g.subscribe(c(space));
        assert!(g.check(&member, &msg(&space, None, data.clone())).is_ok());
        // 멤버가 아닌 피어가 전달한 메시지는 거부(감점).
        assert_eq!(
            g.check(&outsider, &msg(&space, None, data.clone()))
                .unwrap_err(),
            MessageAcceptance::Reject
        );
        // 출처를 실은 메시지는 거부(`Anonymous`).
        assert_eq!(
            g.check(&member, &msg(&space, Some(member), data.clone()))
                .unwrap_err(),
            MessageAcceptance::Reject
        );
        // 해석 불가·다른 공간을 담은 메시지는 거부.
        assert_eq!(
            g.check(&member, &msg(&space, None, vec![0xff; 10]))
                .unwrap_err(),
            MessageAcceptance::Reject
        );
        let s2 = SpaceId::random();
        let other = encode_outgoing(&s2, &sample_events(s2)).unwrap();
        assert_eq!(
            g.check(&member, &msg(&space, None, other)).unwrap_err(),
            MessageAcceptance::Reject
        );
        // 모르는 토픽은 무시.
        assert_eq!(
            g.check(&member, &msg(&SpaceId::random(), None, data.clone()))
                .unwrap_err(),
            MessageAcceptance::Ignore
        );
        // 구독 해제 후에는 무시.
        assert!(g.unsubscribe(&c(space)));
        assert!(!g.is_subscribed(&c(space)));
        assert_eq!(
            g.check(&member, &msg(&space, None, data)).unwrap_err(),
            MessageAcceptance::Ignore
        );
    }

    #[test]
    fn check_without_allowed_list_ignores() {
        // 허용 목록을 정하기 전에 구독했으면 누구의 메시지든 감점 없이 버린다.
        let mut g = GossipState::default();
        let space = SpaceId::random();
        let peer = PeerId::random();
        g.subscribe(c(space));
        let data = encode_outgoing(&space, &sample_events(space)).unwrap();
        assert_eq!(
            g.check(&peer, &msg(&space, None, data.clone()))
                .unwrap_err(),
            MessageAcceptance::Ignore
        );
        g.set_allowed(c(space), vec![peer]);
        assert!(g.check(&peer, &msg(&space, None, data)).is_ok());
    }

    #[test]
    fn outgoing_checks() {
        let space = SpaceId::random();
        let m = sample_events(space);
        assert!(encode_outgoing(&space, &m).is_ok());
        assert!(matches!(
            encode_outgoing(&SpaceId::random(), &m),
            Err(crate::NetError::InvalidInput(_))
        ));
        let empty = GossipMessage::Events {
            space,
            events: vec![],
        };
        assert!(encode_outgoing(&space, &empty).is_err());
    }
}
