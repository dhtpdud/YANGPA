//! 통합 테스트 전용 도우미. 공개 API가 아니며 언제든 바뀝니다.
//!
//! 노드 내부(적용 경로·저장소·키 그룹 상태)를 들여다보거나, 위조·변조 이벤트를 넣어 볼 때 씁니다.
//! 보통의 테스트는 [`crate::Node::call`]로 로컬 API만 부르세요.

use std::sync::Arc;

use yangpa_core::body::{
    EventBody, InviteProof, KeyShareBody, MessageBody, Secret32, SharedKey, state_key_id,
};
use yangpa_core::device::{
    BoxPublicKey, DeviceEntry, DeviceList, DeviceSignKeys, SignedDeviceList,
};
use yangpa_core::event::{EVENT_VERSION, EventHeader, KeyRef, Lane, SignedEvent, StreamId};
use yangpa_core::ids::{
    ChannelId, ChannelRootId, DeviceId, EventId, LinkToken, SpaceId, StateKeyId, UserId,
};
use yangpa_core::model::{
    ChannelSettings, Member, PermissionOverwrite, SpaceKind, SpaceSettings, SpaceState,
};
use yangpa_core::time::Timestamp;
use yangpa_core::wire::{LamportRange, PeerAuth, YpRequest, YpResponse};
use yangpa_crypto::{encrypt_body, seal_body};
use yangpa_net::PeerId;
use yangpa_store::RequestDirection;

pub use crate::blobs::testing as blobs;
pub use crate::ingest::{IngestReport, Source};
pub use crate::spaces::StateWrites;

use crate::author::{self, KeyChoice};
use crate::error::{Error, Result};
use crate::node::Node;
use crate::{ingest, keys, messages, peers};

/// 피어가 보낸 것처럼 이벤트를 적용합니다(인증·멤버 검사 없이 적용 경로만).
pub async fn ingest_from(
    node: &Arc<Node>,
    space: SpaceId,
    events: Vec<SignedEvent>,
    from: PeerId,
) -> Result<IngestReport> {
    ingest::ingest(node, space, events, Source::Peer(from)).await
}

/// 실시간 전파(gossip·`PushEvents`)로 받은 것처럼 적용합니다. 인증·멤버 검사 없이 적용 경로만 타고, 상태·키 갱신 이벤트는
/// 작성 기기별 속도 제한을 거칩니다.
pub async fn ingest_live_from(
    node: &Arc<Node>,
    space: SpaceId,
    events: Vec<SignedEvent>,
    from: PeerId,
) -> Result<IngestReport> {
    ingest::ingest_live(node, space, events, from).await
}

/// 이 노드의 기기로 서명한 본문 없는(머리만) 상태 이벤트 `n`개. 이 노드에 넣지도 전파하지도 않습니다(받는 쪽의
/// 작성 기기별 속도 제한 시험용).
pub fn header_only_state_events(
    node: &Arc<Node>,
    space: SpaceId,
    n: usize,
) -> Result<Vec<SignedEvent>> {
    (0..n)
        .map(|_| {
            let body = EventBody::ChannelCreate {
                channel: ChannelId::random(),
                settings: ChannelSettings::text("머리만"),
            };
            let ev = author::author_event(node, &space, None, &body, &KeyChoice::State, &[])?;
            Ok(ev.without_body())
        })
        .collect()
}

/// 이 노드의 기기로 서명한 본문 없는(머리만) 상태 이벤트 `n`개를 앞 것을 부모로 이어 만들어 이 노드에만 넣습니다
/// (전파하지 않음, 따라잡기로는 내줌). 받는 쪽이 끝 이벤트 하나만 받고 빈 부모를 채우는 시험용.
pub async fn header_only_state_chain(
    node: &Arc<Node>,
    space: SpaceId,
    n: usize,
) -> Result<Vec<SignedEvent>> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let body = EventBody::ChannelCreate {
            channel: ChannelId::random(),
            settings: ChannelSettings::text("머리만"),
        };
        let ev =
            author::author_event(node, &space, None, &body, &KeyChoice::State, &[])?.without_body();
        ingest::ingest(node, space, vec![ev.clone()], Source::Local).await?;
        out.push(ev);
    }
    Ok(out)
}

/// 초대를 받아 준 멤버처럼, 지어낸 가입자 `n`명의 `MemberJoin`을 이 노드의 기기로 서명만 합니다(적용·전파하지 않음).
/// 증명은 `token`으로 만들고, `forge`면 토큰 해시는 같되 다른 토큰의 키로 서명해 확인에 실패하게 합니다. 가입자의
/// 기기 목록은 증명이 묶는 내용일 뿐이라 키 하나로 서명합니다(받는 쪽 작성 기기별 속도 제한 시험용).
pub fn invite_join_events(
    node: &Node,
    space: SpaceId,
    token: &LinkToken,
    n: usize,
    forge: bool,
) -> Result<Vec<SignedEvent>> {
    let keys = yangpa_core::sign::SigningKeypair::generate();
    let other = LinkToken::random();
    (0..n)
        .map(|_| {
            let user = UserId::random();
            let sign_keys = fake_sign_keys();
            let devices = DeviceList {
                user,
                version: 1,
                devices: vec![DeviceEntry {
                    device: sign_keys.device_id(),
                    box_key: BoxPublicKey::filled(3),
                    sign_keys,
                    name: String::new(),
                    added_at: Timestamp::now(),
                    revoked_at: None,
                }],
                otp: None,
            }
            .sign_with(&keys, &keys)?;
            let mut proof = InviteProof::create(
                if forge { &other } else { token },
                &space,
                &devices,
                Timestamp::now(),
            )?;
            proof.token_hash = yangpa_core::links::token_hash(token);
            let body = EventBody::MemberJoin {
                member: Member {
                    user,
                    roles: Vec::new(),
                    nickname: None,
                    automation: false,
                },
                devices,
                profile: None,
                invite: Some(proof),
            };
            author::author_event(node, &space, None, &body, &KeyChoice::State, &[])
        })
        .collect()
}

/// 이 피어와 이 공간을 제한 없이 따라잡습니다(주기 따라잡기처럼, 기다리지 않음).
pub fn spawn_catch_up(node: &Arc<Node>, space: SpaceId, peer: PeerId) {
    crate::sync_service::spawn_catch_up(node, space, peer);
}

/// 저장된 이벤트(본문 포함 여부 그대로), 논리 시계 순서.
pub fn stream_events(node: &Node, space: &SpaceId, stream: StreamId) -> Result<Vec<SignedEvent>> {
    let range = LamportRange {
        start: 0,
        end: u64::MAX,
    };
    Ok(node.store.stream_events(space, stream, range, 4096)?)
}

/// 채널의 메시지 레인 이벤트만.
pub fn message_events(
    node: &Node,
    space: &SpaceId,
    channel: &ChannelId,
) -> Result<Vec<SignedEvent>> {
    Ok(stream_events(node, space, StreamId::Channel(*channel))?
        .into_iter()
        .filter(|e| e.header.kind.lane() == Lane::Message)
        .collect())
}

pub fn get_event(node: &Node, id: &EventId) -> Result<Option<SignedEvent>> {
    Ok(node.store.get_event(id)?)
}

/// 저장소에 본문까지 있는지.
pub fn has_body(node: &Node, id: &EventId) -> bool {
    node.store
        .event_meta(id)
        .ok()
        .flatten()
        .is_some_and(|m| m.has_body)
}

pub fn space_state(node: &Node, space: &SpaceId) -> Option<Arc<SpaceState>> {
    node.space_state(space)
}

/// 그 공간 멤버 쪽 키 그룹 상태를 잠깐 빌려 봅니다(적용 잠금 밖에서도 공간 코어 잠금으로 일관된 값).
fn with_member_keys<T>(
    node: &Node,
    space: &SpaceId,
    f: impl FnOnce(&crate::keygroup::MemberState, &yangpa_sync::Resolver) -> Option<T>,
) -> Option<T> {
    let core = node.spaces.read().get(space).map(|rt| rt.core.clone())?;
    let mut guard = core.lock();
    let c = &mut *guard;
    c.keys.ensure_loaded(node, space);
    f(&c.keys, &c.resolver)
}

/// 이 기기가 가진 크루 키 그룹 사슬 끝 상태의 순번(크루 키를 아직 얻지 못했으면 `None`).
pub fn crew_key_seq(node: &Node, space: &SpaceId) -> Option<u64> {
    with_member_keys(node, space, |k, r| {
        k.crew_current(space, r).map(|m| m.seq())
    })
}

pub fn user_id(node: &Node) -> Option<UserId> {
    node.me().ok().map(|m| m.user())
}

pub fn device_id(node: &Node) -> Option<DeviceId> {
    node.me().ok().map(|m| m.device_id())
}

/// GitHub 후원 확인 연결을 로그인 없이 넣고(`valid_until`까지 유효) 내 다른 기기에 나눕니다.
pub fn set_supporter_link(node: &Arc<Node>, login: &str, valid_until: Timestamp) {
    crate::supporter::put_link_for_test(
        node,
        yangpa_rpc::api::SupporterProvider::Github,
        login,
        valid_until,
        0,
    );
}

/// 월 `monthly_cents`(USD 센트) 정기 후원으로 확인한 GitHub 연결을 넣고 후원자 표시를 냅니다.
pub fn set_supporter_tier(node: &Arc<Node>, monthly_cents: u32) {
    crate::supporter::put_link_for_test(
        node,
        yangpa_rpc::api::SupporterProvider::Github,
        "sponsor",
        Timestamp::now().saturating_add(std::time::Duration::from_secs(3600)),
        monthly_cents,
    );
}

/// 공간에 내지 않고 내 후광 단계만 모읍니다(표시가 빠진 공간이 이미 있는 기기처럼).
pub fn hold_flair(node: &Node) {
    crate::flair::hold_for_test(node);
}

/// 내 후원자 표시가 빠진 공간에 냅니다(후원 확인 주기처럼).
pub async fn spread_flair(node: &Arc<Node>) {
    crate::flair::spread(node, node.space_ids()).await;
}

/// 후원자 확인이 이만큼의 결제를 본 것으로 합니다(후원 뱃지 기준).
pub fn see_support(node: &Node, cents: u64) {
    crate::badges::on_support_seen(node, cents);
}

/// 내 프로필·뱃지 카드가 빠진 공간에 지금 싣습니다(주기 작업을 기다리지 않게).
pub async fn spread_cards(node: &Arc<Node>) {
    crate::cards::spread(node, node.space_ids()).await;
}

/// 이 노드가 키를 기다리며 보관 중인 이벤트 수(RAM 보류 제외).
pub fn waiting_count(node: &Node, space: &SpaceId) -> usize {
    let core = node.spaces.read().get(space).map(|r| r.core.clone());
    core.map(|c| c.lock().waiting.values().map(|s| s.len()).sum())
        .unwrap_or(0)
}

/// `message.send`와 같이 메시지를 만들어 이 노드에만 적용하고 전파하지 않습니다. 채널 뿌리는 키 그룹 키에서 파생하므로
/// 따로 내는 키 묶음은 없고 메시지 하나만 돌려줍니다(순서 바꿔 넣기 시험용).
pub async fn send_unpropagated(
    node: &Arc<Node>,
    space: SpaceId,
    channel: ChannelId,
    text: &str,
) -> Result<Vec<SignedEvent>> {
    let body = EventBody::Message(MessageBody {
        text: text.to_owned(),
        attachments: Vec::new(),
        embeds: Vec::new(),
        reply_to: None,
        thread: None,
        mentions: Vec::new(),
    });
    let l = ingest::lock(node).await;
    let state = node
        .space_state(&space)
        .ok_or_else(|| Error::not_found("모르는 공간"))?;
    let (hid, key) = messages::send_key_locked(node, &l, &space, &channel, &state)?;
    let (ev, _) = author::publish_locked(
        node,
        &l,
        space,
        Some(channel),
        &body,
        KeyChoice::HistoryWith(hid, key),
        &[],
    )?;
    Ok(vec![ev])
}

/// [`send_unpropagated`]와 같되 주어진 채널 뿌리로 암호화합니다(받는 쪽이 파생할 수 없는 뿌리: 그 뿌리를 담은 키
/// 묶음이 와야 풀리는지 볼 때). 이 노드도 그 뿌리를 모르면 자기 메시지를 키 대기로 둡니다.
pub async fn send_unpropagated_with_root(
    node: &Arc<Node>,
    space: SpaceId,
    channel: ChannelId,
    text: &str,
    root: Secret32,
) -> Result<SignedEvent> {
    let body = EventBody::Message(MessageBody {
        text: text.to_owned(),
        attachments: Vec::new(),
        embeds: Vec::new(),
        reply_to: None,
        thread: None,
        mentions: Vec::new(),
    });
    let l = ingest::lock(node).await;
    let id = yangpa_crypto::keysched::channel_root_id(&space, &channel, &root);
    let (ev, _) = author::publish_locked(
        node,
        &l,
        space,
        Some(channel),
        &body,
        KeyChoice::HistoryWith(id, root),
        &[],
    )?;
    Ok(ev)
}

/// 피어와 `Auth`를 주고받습니다(상대가 친구·멤버로 믿지 않으면 오류).
pub async fn authenticate(node: &Arc<Node>, peer: PeerId) -> Result<()> {
    peers::ensure_auth(node, peer).await.map(|_| ())
}

/// 음성·Tor·위치 기록·주소 기록(B5) 시험 도우미.
pub mod net;

/// SFU 통화(X2) 시험 도우미.
pub mod sfu;

/// 노드 간 요청을 그대로 보냅니다(시더 거절 등 확인용).
pub async fn net_request(node: &Node, peer: PeerId, req: YpRequest) -> Result<YpResponse> {
    Ok(node.net.request(peer, req).await?)
}

/// P2P 스웜에서 인증된 피어(P2P 전용 채널 시험용).
pub fn p2p_peers(node: &Node) -> Vec<PeerId> {
    node.peers
        .p2p_peers()
        .into_iter()
        .map(|(p, _, _)| p)
        .collect()
}

/// 파일 회로(`filelink`): (이 기기가 받으려고 이어 둔 회로 수, 이 기기에 들어와 있는 상대 파일 링크 수, 도는 파일 스웜 수).
pub fn file_links(node: &Node) -> (usize, usize, usize) {
    crate::filelink::counts(node)
}

/// 이 기기가 이 상대에게 이어 둔 파일 회로 수.
pub fn file_circuits_to(node: &Node, peer: PeerId) -> usize {
    crate::filelink::ready_circuits(node, &peer).len()
}

/// 받을 자격도 전달할 일도 없는데 받은 남의 부분 전달 이벤트(남의 받은편지함, 볼 수 없는 키 그룹)의 바이트 수
/// (gossip·직접 전송·따라잡기 모두).
pub fn foreign_scoped_bytes(node: &Node) -> u64 {
    node.sync
        .foreign_scoped_bytes
        .load(std::sync::atomic::Ordering::Relaxed)
}

/// 이 기기가 상대 파일 링크로 내준 조각 수.
pub fn file_link_chunks_served(node: &Node) -> u64 {
    crate::filelink::served_chunks(node)
}

/// 예전 빌드처럼 파일 회로 설정을 켜기·끄기(`bool`)로 적습니다(옮겨 읽기 시험용).
pub fn put_legacy_file_circuits(node: &Node, on: bool) {
    node.store
        .put_setting(crate::filelink::SETTING, &on)
        .expect("설정 쓰기");
}

/// 이 P2P 피어와의 연결을 닫습니다(차단하지 않음). VPN을 켜거나 망을 옮겨 끊긴 것을 흉내 냅니다.
pub fn cut_p2p(node: &Node, peer: PeerId) {
    crate::p2p::cut(node, peer)
}

/// 인증된 직접 연결이 끊긴 수.
pub fn p2p_drops(node: &Node) -> u64 {
    node.p2p.drops.load(std::sync::atomic::Ordering::Relaxed)
}

/// P2P 스웜이 도는지.
pub fn p2p_running(node: &Node) -> bool {
    crate::p2p::running(node)
}

/// 네트워크 층이 마지막으로 낸 상태.
pub fn net_status(node: &Node) -> yangpa_net::NetStatus {
    node.net.last_status()
}

/// PeerId를 모르는 주소로 겁니다. 실패하면 오류 문구를 돌려줍니다.
pub async fn dial_addr(
    node: &Node,
    addr: yangpa_net::Multiaddr,
) -> std::result::Result<PeerId, String> {
    node.net.dial_addr(addr).await.map_err(|e| e.to_string())
}

/// 본문 한 바이트를 바꿉니다(본문 해시가 어긋나 서명 검증에 실패해야 함).
pub fn tamper_body(ev: &SignedEvent) -> SignedEvent {
    let mut ev = ev.clone();
    if let Some(b) = ev.body.as_mut()
        && let Some(x) = b.last_mut()
    {
        *x ^= 0x01;
    }
    ev
}

/// 헤더(논리 시계)를 바꿉니다(서명이 맞지 않아야 함).
pub fn tamper_header(ev: &SignedEvent) -> SignedEvent {
    let mut ev = ev.clone();
    ev.header.lamport = ev.header.lamport.wrapping_add(1);
    ev
}

/// `forger` 노드의 기기 키로 서명하고 아무도 모르는 상태 키(무작위 키 ID)로 봉인한 상태 이벤트. 받는 쪽은 풀지
/// 못해 키를 기다리게 됩니다(풀지 못한 상태 이벤트로 체크포인트를 막는 시험용). 넣지도 전파하지도 않습니다.
pub fn forge_unknown_key_state(forger: &Node, space: SpaceId, lamport: u64) -> Result<SignedEvent> {
    let me = forger.me()?;
    let body = EventBody::ChannelCreate {
        channel: ChannelId::random(),
        settings: ChannelSettings::text("가짜"),
    };
    let header = EventHeader {
        v: EVENT_VERSION,
        space,
        channel: None,
        kind: body.kind(),
        author: me.device_id(),
        user: me.user(),
        parents: Vec::new(),
        lamport,
        created_at: Timestamp::now(),
        key: KeyRef::State(StateKeyId::random()),
        body_hash: [0; 32],
        body_len: 0,
        scope: None,
        state_anchor: Vec::new(),
    };
    let bytes = encrypt_body(&Secret32::random(), &header, &body)?;
    SignedEvent::sign(header, bytes, &me.device.signing).map_err(Error::from)
}

/// `forger` 노드의 기기 키로 서명한 메시지를 만듭니다. 기록 키는 아무 값(멤버가 아니라 진짜 키를 모름).
/// `claim_user`를 주면 헤더의 사용자를 그 값으로 적습니다(남의 사용자 ID를 사칭).
pub fn forge_message(
    forger: &Node,
    space: SpaceId,
    channel: ChannelId,
    mut parents: Vec<EventId>,
    lamport: u64,
    text: &str,
    claim_user: Option<UserId>,
) -> Result<SignedEvent> {
    let me = forger.me()?;
    // 서명 때 부모를 정렬하므로 암호화 AD도 정렬된 헤더로 만듭니다.
    parents.sort();
    parents.dedup();
    let body = EventBody::Message(MessageBody {
        text: text.to_owned(),
        attachments: Vec::new(),
        embeds: Vec::new(),
        reply_to: None,
        thread: None,
        mentions: Vec::new(),
    });
    let header = EventHeader {
        v: EVENT_VERSION,
        space,
        channel: Some(channel),
        kind: body.kind(),
        author: me.device_id(),
        user: claim_user.unwrap_or_else(|| me.user()),
        parents,
        lamport,
        created_at: Timestamp::now(),
        key: KeyRef::Record {
            root: ChannelRootId::random(),
        },
        body_hash: [0; 32],
        body_len: 0,
        scope: None,
        state_anchor: Vec::new(),
    };
    let bytes = encrypt_body(&Secret32::random(), &header, &body)?;
    SignedEvent::sign(header, bytes, &me.device.signing).map_err(Error::from)
}

/// 남의 기기(`victim`)가 쓴 것처럼 작성 기기·사용자를 바꾸고 `forger` 키로 짧은 서명을 다시 붙입니다. 짧은 서명에는
/// 공개키가 없어, 받는 쪽이 상태 기록의 진짜 키로 확인해야만 거절됩니다.
pub fn forge_slim_as(
    forger: &Node,
    ev: &SignedEvent,
    victim: (DeviceId, UserId),
) -> Result<SignedEvent> {
    let me = forger.me()?;
    let mut ev = ev.clone();
    ev.header.author = victim.0;
    ev.header.user = victim.1;
    ev.sig = me.device.signing.sign_slim(
        yangpa_core::sign::SignDomain::Event,
        &ev.header.canonical_bytes(),
    );
    Ok(ev)
}

/// 이 노드 저장소가 아는 공간(런타임이 없어도 행이 있으면 포함).
pub fn known_spaces(node: &Node) -> Vec<SpaceId> {
    node.store.space_ids().unwrap_or_default()
}

/// 메시지 레인 본문(수정·삭제·반응 …)을 서명만 하고 이 노드에는 적용하지 않습니다. 받는 쪽 검사를 시험할 때
/// 씁니다. 채널 뿌리는 키 그룹 키에서 파생하므로 이벤트 하나만 돌려줍니다.
pub async fn author_message_body(
    node: &Arc<Node>,
    space: SpaceId,
    channel: ChannelId,
    body: EventBody,
) -> Result<Vec<SignedEvent>> {
    let l = ingest::lock(node).await;
    let state = node
        .space_state(&space)
        .ok_or_else(|| Error::not_found("모르는 공간"))?;
    let (hid, key) = messages::send_key_locked(node, &l, &space, &channel, &state)?;
    let ev = author::author_event(
        node,
        &space,
        Some(channel),
        &body,
        &KeyChoice::HistoryWith(hid, key),
        &[],
    )?;
    Ok(vec![ev])
}

/// 채널을 만들어 적용·전파합니다(크루 영역 전의 시험용). `overwrites`로 볼 수 있는 사람을 정합니다.
pub async fn create_channel(
    node: &Arc<Node>,
    space: SpaceId,
    name: &str,
    overwrites: Vec<PermissionOverwrite>,
) -> Result<ChannelId> {
    let channel = ChannelId::random();
    let mut settings = ChannelSettings::text(name);
    settings.overwrites = overwrites;
    let body = EventBody::ChannelCreate { channel, settings };
    author::publish(node, space, None, &body, KeyChoice::State).await?;
    Ok(channel)
}

/// 키 묶음을 어떻게 암호화할지.
pub enum ShareVia {
    /// 그 채널이 쓰는 키 그룹(전원이 보는 채널이면 크루)의 지금 상태 키.
    GroupState,
    /// 공간 멤버의 활성 기기 모두에게 봉인.
    SealedToMembers,
}

/// 채널 스트림 키 묶음을 서명만 합니다(적용하지 않음).
pub fn author_key_share(
    node: &Node,
    space: SpaceId,
    channel: ChannelId,
    keys: Vec<SharedKey>,
    via: ShareVia,
) -> Result<SignedEvent> {
    let body = EventBody::KeyShare(KeyShareBody {
        keys,
        for_user: None,
    });
    let state = node
        .space_state(&space)
        .ok_or_else(|| Error::not_found("모르는 공간"))?;
    let choice = match via {
        ShareVia::GroupState => {
            let group = crate::keygroup::channel_group(node, &space, &state, &channel);
            let s = with_member_keys(node, &space, |k, r| {
                let m = match &group {
                    None => k.crew_current(&space, r)?,
                    Some(g) => k.group_current(g)?,
                };
                Some(m.state_key())
            })
            .ok_or_else(|| Error::conflict("그 채널의 키 그룹 키가 없음"))?;
            KeyChoice::StateWith(state_key_id(&space, &s), s)
        }
        ShareVia::SealedToMembers => {
            KeyChoice::Sealed(crate::devkeys::sealing_keys(node, state.active_devices()))
        }
    };
    author::author_event(node, &space, Some(channel), &body, &choice, &[])
}

/// 위조한 1:1 공간 묶음: `recipient`에게 봉인한 상태 키 묶음과, 그 키로 연 `SpaceCreate`(소유자를 `claim_user`로
/// 적었지만 `forger`의 기기·기기 목록으로 서명). 소유자의 신원 키가 서명한 기기 목록이 없으므로 해소되지 않아야 합니다.
pub fn forge_friend_space(
    forger: &Node,
    recipient: &Node,
    space: SpaceId,
    claim_user: UserId,
) -> Result<Vec<SignedEvent>> {
    let f = forger.me()?;
    let r = recipient.me()?;
    let key = Secret32::random();
    let kid = state_key_id(&space, &key);
    let header = |kind, key_ref, parents: Vec<EventId>, lamport| EventHeader {
        v: EVENT_VERSION,
        space,
        channel: None,
        kind,
        author: f.device_id(),
        user: claim_user,
        parents,
        lamport,
        created_at: Timestamp::now(),
        key: key_ref,
        body_hash: [0; 32],
        body_len: 0,
        scope: None,
        state_anchor: Vec::new(),
    };
    let share = EventBody::KeyShare(KeyShareBody {
        keys: vec![SharedKey::State {
            id: kid,
            key: key.clone(),
        }],
        for_user: None,
    });
    let h1 = header(share.kind(), KeyRef::Sealed, Vec::new(), 1);
    let b1 = seal_body(&[(r.device_id(), r.device.box_public())], &h1, &share)?;
    let ev1 = SignedEvent::sign(h1, b1, &f.device.signing)?;
    let member = |user| Member {
        user,
        roles: Vec::new(),
        nickname: None,
        automation: false,
    };
    let create = EventBody::SpaceCreate {
        kind: SpaceKind::Space,
        settings: SpaceSettings {
            name: "가짜".into(),
            description: String::new(),
            icon: None,
            icon_key: None,
            allow_external_emoji: false,
            strict_rotation: false,
        },
        owner: claim_user,
        committer: f.device_id(),
        everyone: yangpa_sync::state::FRIEND_SPACE_EVERYONE,
        members: vec![member(claim_user), member(r.user())],
        devices: vec![f.devices.clone(), r.devices.clone()],
    };
    let h2 = header(create.kind(), KeyRef::State(kid), vec![ev1.id()], 2);
    let b2 = encrypt_body(&key, &h2, &create)?;
    let ev2 = SignedEvent::sign(h2, b2, &f.device.signing)?;
    Ok(vec![ev1, ev2])
}

/// 끊어도 1:1 공간을 남기던 때의 데이터를 흉내 냅니다: 공간 상태에서 나 말고 모두 뺍니다.
pub fn leave_only_me(node: &Node, space: &SpaceId) {
    let me = node.me().expect("신원").user();
    let mut spaces = node.spaces.write();
    let rt = spaces.get_mut(space).expect("공간");
    let mut st = (**rt.state.as_ref().expect("상태")).clone();
    let others: Vec<UserId> = st.members.keys().filter(|u| **u != me).copied().collect();
    for u in others {
        st.members.remove(&u);
    }
    rt.state = Some(Arc::new(st));
}

/// 시작할 때 돌리는 한 사람만 남은 1:1 공간 정리.
pub fn sweep_cut(node: &Arc<Node>) {
    crate::friends::spawn_cut_sweep(node);
}

/// 공간 gossip 허용 목록으로 고를 멤버 노드(`sync_service::refresh_space_peers`가 쓰는 것).
pub fn space_member_peers(node: &Node, space: SpaceId) -> Vec<PeerId> {
    node.space_state(&space)
        .map(|st| crate::sync_service::member_peers(node, &st))
        .unwrap_or_default()
}

/// 그 피어와 인증된 연결이 있는지.
pub fn is_authed(node: &Node, peer: PeerId) -> bool {
    node.peers.identity_of(&peer).is_some()
}

/// 그 피어와의 연결을 끊습니다(망이 바뀌어 끊긴 것처럼, 차단은 하지 않음).
pub fn disconnect(node: &Node, peer: PeerId) {
    node.net.disconnect(peer);
}

/// 채널 토픽으로 올린 수(묶음, 묶음 대신 크루, 알림).
pub fn channel_topic_sends(node: &Node) -> (u64, u64, u64) {
    crate::chantopics::sent_counts(node)
}

/// 채널 끝 묻기를 한 번 돌립니다(보통은 다시 걸기 주기마다).
pub async fn poll_channel_tips(node: &Arc<Node>) {
    crate::chantopics::poll_tips(node).await;
}

/// 지금 구독한 채널 묶음 번호.
pub fn channel_buckets(node: &Node, space: SpaceId) -> Vec<u8> {
    crate::chantopics::subscribed_buckets(node, &space)
}

/// 이웃 고르기가 이번 바퀴에 걸 노드.
pub fn neighbor_plan(node: &Arc<Node>) -> Vec<PeerId> {
    crate::neighbors::plan(node)
        .dials
        .into_iter()
        .map(|c| c.peer)
        .collect()
}

/// 그 피어와 연결돼 있는지(인증 여부와 무관).
pub fn is_connected(node: &Node, peer: PeerId) -> bool {
    node.peers.is_connected(&peer)
}

/// 공간 gossip 허용 목록을 비웁니다(그 피어의 PeerId를 모를 때 세운 목록을 흉내).
pub fn clear_gossip_peers(node: &Node, space: SpaceId) {
    node.net
        .set_allowed_peers(yangpa_core::wire::GossipTopic::Crew(space), Vec::new());
}

/// 이미 인증했더라도 `Auth`를 다시 보냅니다(상대 쪽 `handle_auth`가 다시 돌게).
pub async fn reauth(node: &Arc<Node>, peer: PeerId) -> Result<()> {
    let me = node.me()?;
    let auth = PeerAuth::create(
        &me.device.signing,
        me.devices.clone(),
        &node.peer_id().to_bytes(),
        &peer.to_bytes(),
    );
    match node
        .net
        .request(peer, YpRequest::Auth(Box::new(auth)))
        .await?
    {
        YpResponse::Auth(_) => Ok(()),
        other => Err(Error::internal(format!("인증 응답이 아님: {other:?}"))),
    }
}

/// 휘발성 채널(B2) 도우미.
pub mod volatile;

/// 보관 전용 노드(X3) 도우미.
pub mod archive;

/// 상태 체크포인트 도우미.
pub mod checkpoint;

/// Tor로 돌 때의 블롭 내려받기 한도(동시 블롭, 피어당, 블롭당 조각 요청). 직접 연결 스웜이면 `None`.
pub fn tor_download_limits(node: &Node) -> Option<(usize, u32, u32)> {
    crate::tor::tor_download_limits(node).map(|t| (t.active, t.per_peer, t.total))
}

/// 진행 중인 따라잡기가 없는지.
pub fn catch_up_idle(node: &Node) -> bool {
    crate::sync_service::catch_up_idle(node)
}

/// 보낸 친구 요청을 모두 지웁니다(요청을 취소한 쪽을 흉내). 지운 수.
pub fn drop_outgoing_requests(node: &Node) -> Result<usize> {
    let mut n = 0;
    for r in node
        .store
        .list_friend_requests(RequestDirection::Outgoing)?
    {
        if node.store.delete_friend_request(&r.request_id)? {
            n += 1;
        }
    }
    Ok(n)
}

// ───────────── B4(관리·기기) 시험 도우미 ─────────────

/// 확인 요청 유효 시간을 바꿉니다(만료 시험용).
pub fn set_confirm_ttl(node: &Node, ttl: std::time::Duration) {
    node.admin.set_confirm_ttl(ttl);
}

/// 크루 키 그룹의 지금 구성 기기(해소기의 크루 사슬 끝). 이 기기가 크루 키를 얻지 못했으면 `None`.
pub fn crew_key_members(node: &Node, space: &SpaceId) -> Option<Vec<DeviceId>> {
    with_member_keys(node, space, |k, r| {
        k.crew_current(space, r)?;
        Some(r.key_view()?.crew.members.iter().copied().collect())
    })
}

/// 서명된 기기 목록의 키를 기기 키 표에 넣습니다(그 목록을 실은 상태 이벤트를 받아 저장한 것처럼, 해소기가 받든 말든).
pub fn learn_device_list(node: &Node, space: SpaceId, list: &SignedDeviceList) {
    let body = EventBody::DeviceListUpdate {
        devices: list.clone(),
    };
    node.store
        .put_device_keys(&space, &crate::devkeys::body_keys(&body))
        .expect("기기 키 저장");
}

/// 이 런타임이 그 공간 상태를 저장한 횟수와 행 수(통째로 다시 쓰지 않고 바뀐 행만 쓰는지 볼 때).
pub fn state_writes(node: &Node, space: SpaceId) -> Option<StateWrites> {
    let core = node.spaces.read().get(&space)?.core.clone();
    let w = core.lock().state_writes;
    Some(w)
}

/// 지금 상태에서 그 멤버 기기 기록이 가리키는 키 약속.
pub fn state_key_commit(
    node: &Node,
    space: SpaceId,
    user: UserId,
    device: DeviceId,
) -> Option<[u8; 32]> {
    let state = node.space_state(&space)?;
    state.devices.get(&user)?.get(&device).map(|r| r.key_commit)
}

/// 이 기기에서 공간 데이터를 모두 지웁니다(탈퇴가 전해진 뒤처럼, `crew.leave`의 마지막 단계).
pub async fn purge_space(node: &Arc<Node>, space: SpaceId) -> Result<()> {
    crate::crews::purge_local(node, &space).await
}

/// 그 공간의 적용 잠금을 쥔 채 두는 작업(큰 크루의 긴 적용 흉내). 잠금을 쥔 뒤 돌아오고, 돌려준 것을 놓으면 풉니다.
pub async fn hold_space_lock(node: &Arc<Node>, space: SpaceId) -> tokio::sync::oneshot::Sender<()> {
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let (held_tx, held) = tokio::sync::oneshot::channel::<()>();
    let node = node.clone();
    tokio::spawn(async move {
        let l = crate::ingest::lock_space(&node, space).await;
        let _ = held_tx.send(());
        let _ = released.await;
        drop(l);
    });
    let _ = held.await;
    release
}

/// 그 공간의 `SpaceCore`를 다른 스레드에서 쥔 채 두기(큰 크루의 긴 적용 흉내: 적용은 `SpaceCore`를 적용 내내 쥠).
/// 쥔 뒤 돌아오고, 돌려준 것을 놓으면 풉니다.
pub fn hold_space_core(node: &Node, space: SpaceId) -> std::sync::mpsc::Sender<()> {
    let core = node
        .spaces
        .read()
        .get(&space)
        .map(|rt| rt.core.clone())
        .expect("공간 런타임");
    let (release, released) = std::sync::mpsc::channel::<()>();
    let (held_tx, held) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let _core = core.lock();
        let _ = held_tx.send(());
        let _ = released.recv();
    });
    let _ = held.recv();
    release
}

/// 따라잡기가 지문 비교에서 부분 전달 스트림 ID를 이미 가진 것으로 보는지(`sync_service::have_events`가 부르는 것).
pub fn scoped_known_ids(node: &Node, space: SpaceId, ids: &[EventId]) -> Vec<bool> {
    crate::scoped::known_ids(node, &space, ids)
}

/// 따라잡기 한 바퀴가 지금 끝났다고 적습니다(따라잡기 작업이 끝에 부르는 것).
pub fn note_caught_up(node: &Arc<Node>, space: SpaceId) {
    crate::ingest::note_caught_up(node, space, std::time::Instant::now());
}

/// 새 공간 ID `n`개로 차례로 잠갔다 푼 뒤 공간 잠금 표에 남은 수(쥐는 쪽이 없는 잠금이 쌓이지 않는지).
pub async fn space_lock_table_after(node: &Node, n: usize) -> usize {
    for _ in 0..n {
        drop(crate::ingest::lock_space(node, SpaceId::random()).await);
    }
    node.ingest_spaces.lock().len()
}

/// 기기 키 표 LRU를 비웁니다(DB 행만으로 봉인하는지 볼 때).
pub fn clear_device_key_cache(node: &Node) {
    node.devkeys.clear();
}

/// 짧은 서명을 확인하려고 최근 피어에게 서명 키를 물은 기기 수.
pub fn sign_keys_asked(node: &Node) -> usize {
    node.devkeys.asked_count()
}

/// 기기 키 표(DB)에 그 키 약속으로 저장된 상자 키.
pub fn stored_box_key(node: &Node, key_commit: [u8; 32]) -> Option<BoxPublicKey> {
    node.store
        .device_keys(&[key_commit])
        .ok()?
        .remove(&key_commit)
        .map(|k| k.box_key)
}

/// 지금 상태에서 그 멤버 기기에 봉인할 때 쓰는 상자 키(상태 기록의 키 약속으로 키 표에서 찾음).
pub fn sealing_box_key(
    node: &Node,
    space: SpaceId,
    user: UserId,
    device: DeviceId,
) -> Option<BoxPublicKey> {
    let state = node.space_state(&space)?;
    crate::devkeys::device_box_key(node, &state, &user, &device)
}

/// 이 노드가 저장해 둔 그 사용자의 기기 목록(내 것이면 지금 쓰는 목록).
pub fn device_list(node: &Node, user: &UserId) -> Option<yangpa_core::device::DeviceList> {
    let me = node.me().ok()?;
    if me.user() == *user {
        return Some(me.devices.list.clone());
    }
    node.store
        .get_device_list(user)
        .ok()
        .flatten()
        .map(|l| l.list)
}

/// 크루를 만들어 다른 노드에도 심습니다(크루 영역(B1) 전의 시험용). `owner`가 `others`를 처음 멤버로 넣어 첫 채널이
/// 있는 크루를 만들고, 공간 스트림 이벤트를 각 노드에 그대로 넣습니다. 나머지(채널 키 등)는 평소처럼 연결·인증 뒤
/// 따라잡기로 받습니다.
pub async fn seed_crew(owner: &Arc<Node>, others: &[Arc<Node>], name: &str) -> Result<SpaceId> {
    let me = owner.me()?;
    let member = |user: UserId| Member {
        user,
        roles: Vec::new(),
        nickname: None,
        automation: false,
    };
    let mut members = vec![member(me.user())];
    let mut devices = vec![me.devices.clone()];
    for n in others {
        let m = n.me()?;
        members.push(member(m.user()));
        devices.push(m.devices.clone());
    }
    let spec = crate::spaces::NewSpace {
        kind: SpaceKind::Crew,
        settings: SpaceSettings {
            name: name.into(),
            description: String::new(),
            icon: None,
            icon_key: None,
            allow_external_emoji: false,
            strict_rotation: false,
        },
        everyone: yangpa_core::model::Permissions::DEFAULT_EVERYONE,
        members,
        devices,
        first_channel: Some("일반".into()),
    };
    let l = ingest::lock(owner).await;
    let (space, _evs, report) = crate::spaces::create_space_locked(owner, &l, spec)?;
    drop(l);
    ingest::after_unlock(owner, space, &report);
    let genesis = owner
        .spaces
        .read()
        .get(&space)
        .and_then(|rt| rt.genesis)
        .ok_or_else(|| Error::internal("만든 크루의 시작 이벤트 없음"))?;
    let events = stream_events(owner, &space, StreamId::Space)?;
    for n in others {
        crate::spaces::ensure_runtime(n, &space, Some(genesis))?;
        ingest::ingest(n, space, events.clone(), Source::Bootstrap(owner.peer_id())).await?;
        crate::sync_service::refresh_space_peers(n, &space);
    }
    crate::sync_service::refresh_space_peers(owner, &space);
    Ok(space)
}

/// 아무 바이트로 채운 기기 서명 공개키(기기 ID는 여기서 셈하므로 구조 검사는 통과). 실제 키 쌍을 만들지 않아
/// 빠릅니다. 이 키로는 서명할 수 없습니다.
pub fn fake_sign_keys() -> DeviceSignKeys {
    let mut pq = vec![0u8; yangpa_core::sign::PQ_PUBLIC_LEN];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut pq);
    DeviceSignKeys {
        ed_pk: rand::random(),
        pq_pk: pq.into(),
    }
}

/// 이 노드의 신원 키로, 지금 목록에 기기 `extra`개를 덧붙인 판을 서명합니다(상한을 넘는 목록 시험용).
pub fn padded_device_list(node: &Node, extra: usize) -> Result<SignedDeviceList> {
    let me = node.me()?;
    let mut list = me.devices.list.clone();
    for _ in 0..extra {
        let sign_keys = fake_sign_keys();
        list.devices.push(DeviceEntry {
            device: sign_keys.device_id(),
            box_key: BoxPublicKey::filled(0),
            sign_keys,
            name: "덧붙인 기기".into(),
            added_at: Timestamp::now(),
            revoked_at: None,
        });
    }
    list.version = list.version.saturating_add(1);
    Ok(list.sign_with(&me.identity.signing, &me.device.signing)?)
}

/// 시험에서 붙잡아 둔 신원 키·기기 키(폐기된 기기가 데이터를 지운 뒤에도 그 키로 위조해 보려고).
#[derive(Clone)]
pub struct CapturedKeys {
    identity: yangpa_crypto::Identity,
    device: yangpa_crypto::DeviceKeys,
}

impl CapturedKeys {
    /// 붙잡은 기기 키의 기기 ID.
    pub fn device_id(&self) -> DeviceId {
        self.device.device_id()
    }

    /// 아무 목록이나 붙잡은 신원 키로 서명하고 붙잡은 기기 키로 공동 서명합니다(위조 시험용).
    pub fn sign(&self, list: yangpa_core::device::DeviceList) -> Result<SignedDeviceList> {
        Ok(list.sign_with(&self.identity.signing, &self.device.signing)?)
    }
}

/// 피어가 `Revoked{devices}`로 답한 것처럼 반영합니다(`identity::on_revoked`).
pub fn on_revoked(node: &Arc<Node>, peer: PeerId, devices: &SignedDeviceList) {
    crate::identity::on_revoked(node, peer, devices);
}

/// 이 노드의 기기로 서명한 키 갱신(내용은 쓰레기, 순번 `seq`)을 이 노드에 넣습니다. `channel`이 있으면 그 채널이 쓰는
/// 키 그룹 스트림에, 없으면 크루 갱신으로 공간 스트림에 둡니다. 확정 기기가 아니면 해소기·그룹 사슬이 받지 않지만
/// 저장되고 동기화로 퍼질 수 있습니다(받지 않은 갱신의 순번을 믿는지 볼 때).
pub async fn plant_key_update(
    node: &Arc<Node>,
    space: SpaceId,
    channel: Option<ChannelId>,
    seq: u64,
) -> Result<SignedEvent> {
    use yangpa_core::keyupdate::{KeyUpdateKind, KeyUpdatePayload, RCt};
    let state = node
        .space_state(&space)
        .ok_or_else(|| Error::not_found("모르는 공간"))?;
    let group = channel
        .and_then(|c| crate::keygroup::channel_group(node, &space, &state, &c))
        .unwrap_or_else(|| yangpa_core::ids::KeyGroupId::crew(&space));
    let payload = KeyUpdatePayload {
        group,
        seq,
        prev: None,
        kind: KeyUpdateKind::Refresh,
        root_commit: [7; 32],
        prev_root_commit: [0; 32],
        crew: None,
        bundle_root: None,
        shards: None,
        depth: 0,
        salt: [7; 24],
        layout: Vec::new(),
        wraps: Vec::new(),
        sealed: Vec::new(),
        r_ct: Some(RCt { ct: [7; 48] }),
        part: 0,
        parts: 1,
    };
    let body = EventBody::KeyUpdate(payload);
    let ev = if group == yangpa_core::ids::KeyGroupId::crew(&space) {
        author::author_event(node, &space, None, &body, &KeyChoice::State, &[])?
    } else {
        let (sid, s) = crate::keygroup::current_crew_key(node, &space)
            .ok_or_else(|| Error::conflict("크루 키가 없음"))?;
        author::author_scoped(
            node,
            &space,
            yangpa_core::event::Scope::KeyGroup(group),
            &body,
            &KeyChoice::StateWith(sid, s),
            &[],
            &[],
        )?
    };
    ingest::ingest(node, space, vec![ev.clone()], Source::Local).await?;
    Ok(ev)
}

/// 이 노드의 신원 키·기기 키를 붙잡아 둡니다.
pub fn capture_keys(node: &Node) -> Result<CapturedKeys> {
    let me = node.me()?;
    Ok(CapturedKeys {
        identity: me.identity.clone(),
        device: me.device.clone(),
    })
}

/// 다른 기기의 신원 시드를 훔친 것처럼 신원이 없는 `thief`에 같은 신원을 설치합니다: 새 기기 키를 만들어 아는 목록에 자기만
/// 더한 판을 씁니다. 살아 있는 기기의 공동 서명이 없어 아무도 받지 않습니다(아무와도 잇지 않은 같은 신원의 기기로도 씀).
pub fn install_stolen_seed(victim: &Node, thief: &Arc<Node>) -> Result<DeviceId> {
    let me = victim.me()?;
    let device = yangpa_crypto::DeviceKeys::generate();
    let id = device.device_id();
    let entry = crate::identity::device_entry(thief, &device, Timestamp::now());
    let devices = me
        .devices
        .list
        .with_entry(entry)
        .sign_with(&me.identity.signing, &device.signing)?;
    crate::identity::install_linked(
        thief,
        me.identity.clone(),
        device,
        devices,
        me.profile.clone(),
    )?;
    Ok(id)
}

/// 띄운 기기 연결 링크의 기한을 지난 것으로 만듭니다.
pub fn expire_device_link(node: &Node) {
    crate::devlink::expire(node);
}

/// 이 노드의 신원 키·기기 키로 `base`에 새 기기 하나를 더한 판을 공동 서명합니다. 더한 기기 ID도 돌려줍니다.
pub fn forge_device_list(
    node: &Node,
    base: &yangpa_core::device::DeviceList,
) -> Result<(SignedDeviceList, DeviceId)> {
    forge_device_list_with(&capture_keys(node)?, base)
}

/// 붙잡아 둔 신원 키·기기 키로 `base`에 새 기기 하나를 더한 판을 공동 서명합니다(폐기된 기기가 신원 키로 기기를
/// 더하려는 위조 시험용). 더한 기기 ID도 돌려줍니다.
pub fn forge_device_list_with(
    keys: &CapturedKeys,
    base: &yangpa_core::device::DeviceList,
) -> Result<(SignedDeviceList, DeviceId)> {
    let mut list = base.clone();
    let sign_keys = fake_sign_keys();
    let extra = sign_keys.device_id();
    list.devices.push(DeviceEntry {
        device: extra,
        box_key: BoxPublicKey::filled(9),
        sign_keys,
        name: "위조 기기".into(),
        added_at: Timestamp::now(),
        revoked_at: None,
    });
    list.version = list.version.saturating_add(1);
    Ok((
        list.sign_with(&keys.identity.signing, &keys.device.signing)?,
        extra,
    ))
}

/// 받은 기기 목록처럼 반영합니다(`identity::accept_device_list`). 바뀌었으면 true.
pub fn accept_device_list(node: &Arc<Node>, l: &SignedDeviceList) -> Result<bool> {
    crate::identity::accept_device_list(node, l)
}

/// 이 노드가 저장한(내 것이면 지금 쓰는) 그 사용자의 서명된 기기 목록.
pub fn signed_device_list(node: &Node, user: &UserId) -> Option<SignedDeviceList> {
    match node.me() {
        Ok(me) if me.user() == *user => Some(me.devices.clone()),
        _ => node.store.get_device_list(user).ok().flatten(),
    }
}

/// 받아들인 공간의 주기 작업(오래 해소되지 않은 공간 포기)을 한 번 돌립니다.
pub fn adoption_tick(node: &Arc<Node>) {
    crate::identity::adoption_tick(node);
}

/// 신원이 지워졌는지(폐기 확인 뒤 로컬 데이터 지우기).
pub fn is_wiped(node: &Node) -> bool {
    node.me().is_err()
        && node
            .store
            .get_secret(yangpa_store::secret_names::IDENTITY)
            .ok()
            .flatten()
            .is_none()
}

/// 다음 바퀴에 크루 전체 재키를 하게 하고 바퀴를 바로 돌립니다(확정 기기에서).
pub fn force_full_rekey(node: &Arc<Node>, space: SpaceId) {
    node.keygroup.force_full.lock().insert(space);
    crate::keygroup::urgent(node, space);
}

/// 가입자에게 주는 지난 상태 키 수를 바꿉니다(0이면 기본값): 가입자가 옛 상태 키를 묻는 길을 짧은 기록으로 봅니다.
pub fn set_joiner_state_keys(node: &Node, n: usize) {
    node.keygroup
        .joiner_keys
        .store(n, std::sync::atomic::Ordering::Relaxed);
}

/// 이 기기의 지금 크루 상태 키 ID.
pub fn current_state_key_id(node: &Node, space: &SpaceId) -> Option<StateKeyId> {
    crate::keys::current_state_key(node, space)
        .ok()
        .flatten()
        .map(|(id, _)| id)
}

/// 이 기기가 가진 크루 상태 키 수.
pub fn state_key_count(node: &Node, space: &SpaceId) -> usize {
    crate::keys::all_state_keys(node, space).map_or(0, |v| v.len())
}

/// 확정 기기의 키 갱신 바퀴를 미루거나(`true`) 다시 돌립니다(동시 가입을 갱신 하나로 묶는지 볼 때).
pub fn hold_commits(node: &Node, hold: bool) {
    crate::keygroup::set_hold(node, hold);
}

/// 해소기가 받아들인 크루 키 갱신: (갱신 수, 갱신마다 넣은 기기). 조각으로 나뉜 갱신은 한 번으로 셉니다.
pub fn crew_key_updates(node: &Node, space: &SpaceId) -> (usize, Vec<Vec<DeviceId>>) {
    let Some(core) = node.spaces.read().get(space).map(|rt| rt.core.clone()) else {
        return (0, Vec::new());
    };
    let accepted: std::collections::HashSet<EventId> =
        core.lock().resolver.key_events().iter().copied().collect();
    // 받아들인 것은 마지막 조각이라, 같은 (작성 기기, 순번, salt)의 조각에서 넣은 기기를 모읍니다.
    // (작성 기기, 순번, salt) → (받아들인 조각이 있는지, 넣은 기기).
    type Part = (DeviceId, u64, [u8; 24]);
    let mut parts: std::collections::BTreeMap<Part, (bool, Vec<DeviceId>)> =
        std::collections::BTreeMap::new();
    for ev in node
        .store
        .events_by_lane(space, &[Lane::KeyAgreement])
        .unwrap_or_default()
    {
        if ev.header.scope.is_some() {
            continue;
        }
        let (Some(bytes), KeyRef::State(kid)) = (ev.body.as_deref(), ev.header.key) else {
            continue;
        };
        let Ok(Some(key)) = keys::crew_state_key(node, space, &kid) else {
            continue;
        };
        let Ok(EventBody::KeyUpdate(p)) = yangpa_crypto::decrypt_body(&key, &ev.header, bytes)
        else {
            continue;
        };
        let e = parts.entry((ev.header.author, p.seq, p.salt)).or_default();
        e.0 |= accepted.contains(&ev.id());
        e.1.extend_from_slice(p.kind.joined());
    }
    let joins: Vec<Vec<DeviceId>> = parts
        .into_values()
        .filter(|(ok, _)| *ok)
        .map(|(_, j)| j)
        .collect();
    (joins.len(), joins)
}

/// 공간의 지금 커밋 확정 기기.
pub fn committer_of(node: &Node, space: &SpaceId) -> Option<DeviceId> {
    node.space_state(space).map(|st| st.committer)
}

/// 이 사용자에게 전할 친구 삭제가 남았는지.
pub fn has_pending_leave(node: &Node, user: &UserId) -> bool {
    crate::friends::has_pending_leave(node, user)
}

/// 받아들였지만 아직 상태가 없는 공간인지.
pub fn is_adopting(node: &Node, space: &SpaceId, from: &UserId) -> bool {
    crate::identity::is_adopting(node, space, from)
}

/// 대상이 남의 것일 때 쓰는 사람 확인 경로를 그대로 부릅니다(휘발성 채널 삭제처럼 아직 API가 없는 대상 시험용).
pub async fn confirm_when_others(
    node: &Arc<Node>,
    ctx: &crate::CallCtx,
    method: &str,
    params: &serde_json::Value,
) -> std::result::Result<(), yangpa_rpc::RpcError> {
    crate::admin::confirm_when_others(node, ctx, method, params).await
}

/// RAM에만 둔 확인 요청 수, 저장소에 둔 확인 요청 수.
pub fn confirmation_counts(node: &Node) -> (usize, usize) {
    let stored = node
        .store
        .list_confirmations()
        .map(|v| v.len())
        .unwrap_or(0);
    (crate::admin::volatile_pending_count(node), stored)
}

/// 휘발성 채널을 만들어 적용·전파합니다(크루 영역 전의 시험용).
pub async fn create_volatile_channel(
    node: &Arc<Node>,
    space: SpaceId,
    name: &str,
) -> Result<ChannelId> {
    let channel = ChannelId::random();
    let mut settings = ChannelSettings::text(name);
    settings.security = yangpa_core::model::SecurityLevel::Volatile;
    let body = EventBody::ChannelCreate { channel, settings };
    author::publish(node, space, None, &body, KeyChoice::State).await?;
    Ok(channel)
}

/// 이 공간의 상태 키를 모두 주어진 기기들에게 봉인한 키 묶음을 서명만 합니다(적용·전파하지 않음).
pub fn author_sealed_state_keys(
    node: &Node,
    space: SpaceId,
    to: Vec<(DeviceId, BoxPublicKey)>,
) -> Result<SignedEvent> {
    let body = EventBody::KeyShare(KeyShareBody {
        keys: keys::all_state_keys(node, &space)?,
        for_user: None,
    });
    author::author_event(node, &space, None, &body, &KeyChoice::Sealed(to), &[])
}

/// 이 기기의 상자 공개키.
pub fn box_public(node: &Node) -> Option<BoxPublicKey> {
    node.me().ok().map(|m| m.device.box_public())
}

/// 공간의 시작 이벤트.
pub fn genesis_of(node: &Node, space: &SpaceId) -> Option<EventId> {
    node.spaces.read().get(space).and_then(|rt| rt.genesis)
}

/// 시작 이벤트로 고정한 빈 런타임을 만듭니다(기기 연결로 공간을 받아들인 것처럼).
pub fn pin_space(node: &Arc<Node>, space: SpaceId, genesis: EventId) -> Result<()> {
    crate::spaces::ensure_runtime(node, &space, Some(genesis))?;
    Ok(())
}

// ───────────── B1(크루) 시험 도우미 ─────────────

/// 메시지 레인 본문(게시글 등)을 `message.send`처럼 만들어 이 노드에만 적용하고 전파하지 않습니다. 채널 뿌리는 키
/// 그룹 키에서 파생하므로 이벤트 하나만 돌려줍니다(순서 바꿔 넣기 시험용).
pub async fn publish_message_unpropagated(
    node: &Arc<Node>,
    space: SpaceId,
    channel: ChannelId,
    body: EventBody,
) -> Result<Vec<SignedEvent>> {
    let l = ingest::lock(node).await;
    let state = node
        .space_state(&space)
        .ok_or_else(|| Error::not_found("모르는 공간"))?;
    let (hid, key) = messages::send_key_locked(node, &l, &space, &channel, &state)?;
    let (ev, _) = author::publish_locked(
        node,
        &l,
        space,
        Some(channel),
        &body,
        KeyChoice::HistoryWith(hid, key),
        &[],
    )?;
    Ok(vec![ev])
}

/// 상태 레인 본문을 지금 상태 키로 만들어 이 노드에만 적용하고 전파하지 않습니다(적용이 거부되면 오류).
pub async fn publish_state_unpropagated(
    node: &Arc<Node>,
    space: SpaceId,
    channel: Option<ChannelId>,
    body: EventBody,
) -> Result<SignedEvent> {
    let l = ingest::lock(node).await;
    let (ev, _) = author::publish_locked(node, &l, space, channel, &body, KeyChoice::State, &[])?;
    Ok(ev)
}

/// 상태 레인 본문을 지금 상태 키로 서명만 합니다(이 노드에 적용하지 않음). 권한 없는 멤버가 만든 위조 상태
/// 이벤트를 다른 노드에 넣어 볼 때 씁니다.
pub fn author_state_event(
    node: &Node,
    space: SpaceId,
    channel: Option<ChannelId>,
    body: &EventBody,
) -> Result<SignedEvent> {
    author::author_event(node, &space, channel, body, &KeyChoice::State, &[])
}

/// 상태 레인 본문을 지정한 상태 키로 서명만 합니다(이 노드에 적용하지 않음). 다른 멤버가 갖지 않은 키로 쓴 상태
/// 이벤트를 넣어 볼 때 씁니다.
pub fn author_state_event_with(
    node: &Node,
    space: SpaceId,
    body: &EventBody,
    key: Secret32,
) -> Result<SignedEvent> {
    let id = state_key_id(&space, &key);
    author::author_event(
        node,
        &space,
        None,
        body,
        &KeyChoice::StateWith(id, key),
        &[],
    )
}

/// `to`의 받은편지함에 키 묶음을 서명만 합니다(이 노드에 적용하지 않음). `anchor`는 기준점 앞자리에 넣을 이벤트입니다.
pub fn author_inbox_share(
    node: &Node,
    space: SpaceId,
    to: UserId,
    rcpts: Vec<(DeviceId, BoxPublicKey)>,
    keys: Vec<SharedKey>,
    anchor: &[EventId],
) -> Result<SignedEvent> {
    let body = EventBody::KeyShare(KeyShareBody {
        keys,
        for_user: Some(to),
    });
    author::author_scoped(
        node,
        &space,
        yangpa_core::event::Scope::Inbox(to),
        &body,
        &KeyChoice::Sealed(rcpts),
        anchor,
        &[],
    )
}

/// 그 채널이 쓰는 키 그룹(`None`이거나 전원이 보는 채널이면 크루)의 지금 구성 기기. 이 기기가 그 그룹 키를 갖지
/// 않거나 모르는 채널이면 `None`.
pub fn key_group_members(
    node: &Node,
    space: &SpaceId,
    channel: Option<ChannelId>,
) -> Option<Vec<DeviceId>> {
    let group = match channel {
        None => None,
        Some(c) => {
            let state = node.space_state(space)?;
            state.channels.get(&c).filter(|ch| ch.uses_group_keys())?;
            crate::keygroup::channel_group(node, space, &state, &c)
        }
    };
    let Some(group) = group else {
        return crew_key_members(node, space);
    };
    with_member_keys(node, space, |k, _| {
        k.group_current(&group)?;
        Some(
            k.chains
                .get(&group)?
                .tip()
                .members
                .iter()
                .copied()
                .collect(),
        )
    })
}

/// 그 채널이 쓰는 키 그룹 사슬 끝의 내 상태 순번(전원이 보는 채널이면 크루 순번). 그룹 키가 없으면 `None`.
pub fn channel_key_seq(node: &Node, space: &SpaceId, channel: ChannelId) -> Option<u64> {
    let state = node.space_state(space)?;
    // 모르는 채널(아직 상태를 받지 못함)이나 그룹이 없는 채널(휘발성·카테고리)은 `None`.
    state
        .channels
        .get(&channel)
        .filter(|ch| ch.uses_group_keys())?;
    match crate::keygroup::channel_group(node, space, &state, &channel) {
        None => crew_key_seq(node, space),
        Some(g) => with_member_keys(node, space, |k, _| k.group_current(&g).map(|m| m.seq())),
    }
}

/// 이 메시지가 크루 그룹 키에서 나온 채널 뿌리로 암호화됐는지(이 기기가 보관한 크루 상태들로 파생해 봄).
pub fn uses_crew_root(node: &Node, header: &EventHeader) -> bool {
    let (KeyRef::Record { root }, Some(channel)) = (header.key, header.channel) else {
        return false;
    };
    let space = header.space;
    with_member_keys(node, &space, |k, _| {
        let crew = k.groups.get(&yangpa_core::ids::KeyGroupId::crew(&space))?;
        crew.states()
            .any(|m| {
                [m.public_root(&channel), m.private_root(&channel)]
                    .iter()
                    .any(|r| yangpa_crypto::keysched::channel_root_id(&space, &channel, r) == root)
            })
            .then_some(())
    })
    .is_some()
}

/// 초대 가입 대기 중인지(가입 키 갱신을 기다림).
pub fn is_pending_join(node: &Node, space: &SpaceId) -> bool {
    crate::crews::is_pending_join(node, space)
}

/// 이 노드가 메시지를 풀어 보기 행으로 가지고 있으면 그 본문(권한과 상관없이 저장소를 직접 봄).
pub fn message_text(node: &Node, id: &EventId) -> Option<String> {
    node.store
        .get_message(id, None)
        .ok()
        .flatten()
        .and_then(|r| r.content)
        .map(|c| c.body.text)
}

// ───────────── 모더레이션(W2) ─────────────

/// 이 노드의 서명된 기기 목록(위조 가입 이벤트를 만들 때).
pub fn signed_devices(node: &Node) -> Option<SignedDeviceList> {
    node.me().ok().map(|m| m.devices.clone())
}

/// 아직 전하지 못한 신고·처리 결과 묶음 수(디스크 + RAM).
pub fn moderation_outbox_len(node: &Node) -> usize {
    crate::moderation::outbox_len(node)
}

/// 이 노드에 남은 신고 기록 수(저장소 + RAM). 운영진이 아닌 노드는 0이어야 합니다.
pub fn report_count(node: &Node, space: &SpaceId) -> usize {
    crate::moderation::report_count(node, space)
}

/// 이 공간에 대해 저장소에 남은 모더레이션 기록 수(공간을 지운 뒤 0이어야 함).
pub fn moderation_stored_for_space(node: &Node, space: &SpaceId) -> usize {
    crate::moderation::stored_for_space(node, space)
}

/// RAM에 든 차단 목록 크기(기기를 폐기해 지운 뒤 0이어야 함).
pub fn blocked_in_memory(node: &Node) -> usize {
    crate::moderation::blocked_len(node)
}

/// 예전 빌드가 남긴 OTP 비밀처럼 적습니다(시작할 때 지우는지 볼 때).
pub fn plant_old_otp(node: &Node, user: &UserId) {
    for key in ["otp.mine".to_owned(), format!("otp.escrow.{user}")] {
        node.store.put_setting(&key, &1).unwrap();
    }
}

/// 남아 있는 옛 OTP 비밀 수.
pub fn old_otp(node: &Node) -> usize {
    node.store.settings_with_prefix("otp.").unwrap().len()
}

/// 아직 링크 주인에게 전하지 못한 친구 요청 수.
pub fn pending_redeems(node: &Node) -> usize {
    node.store
        .settings_with_prefix("friend.pending_redeem.")
        .map(|v| v.len())
        .unwrap_or(0)
}

/// 예전 판이 남긴 것처럼, 공간 시더 설정의 송신 상한(256KiB/s)과 전역 피어 수신 상한(512KiB/s)을 예전 기본값으로
/// 저장하고 옮김 표시를 지웁니다(다음 시작 때 옮겨지는지 보려고).
pub fn plant_legacy_rates(node: &Node, space: &SpaceId) -> Result<()> {
    use yangpa_sync::policy::{PeerPolicy, SeederPolicy};
    let seed = SeederPolicy {
        upload_limit_bps: Some(256 * 1024),
        ..SeederPolicy::default()
    };
    node.store
        .put_setting(&format!("seed.policy.{space}"), &seed)?;
    let peer = PeerPolicy {
        download_limit_bps: Some(512 * 1024),
        ..PeerPolicy::default()
    };
    node.store.put_setting("peer.policy", &peer)?;
    node.store.delete_setting("policy.rate_defaults_v2")?;
    Ok(())
}

/// 아직 상대에게 전하지 못한 친구 수락 수.
pub fn pending_accepts(node: &Node) -> usize {
    node.store
        .settings_with_prefix("friend.pending_accept.")
        .map(|v| v.len())
        .unwrap_or(0)
}

/// 그 채널의 비공개 키 그룹(`SpaceState::channel_key_group`, 모든 멤버가 보는 채널은 `None`).
pub fn channel_key_group(
    node: &Node,
    space: SpaceId,
    channel: ChannelId,
) -> Option<yangpa_core::ids::KeyGroupId> {
    let st = node.space_state(&space)?;
    st.channel_key_group(&space, st.channels.get(&channel)?)
}

/// 이 노드가 그 공간에서 구독한 그룹 gossip 토픽의 그룹.
pub fn group_topics(node: &Node, space: SpaceId) -> Vec<yangpa_core::ids::KeyGroupId> {
    let Some(st) = node.space_state(&space) else {
        return Vec::new();
    };
    st.channels
        .values()
        .filter_map(|ch| st.channel_key_group(&space, ch))
        .filter(|g| crate::topics::subscribed(node, &space, g))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// 확정 기기로서 키 그룹 스트림에 키 묶음 하나(그 그룹의 새 R 하나)를 내고 전파합니다(지금 그 그룹을 볼 수 있는
/// 멤버의 활성 기기에게 봉인). 키 그룹 갱신과 따로 전달·따라잡기·시더 경로를 직접 시험합니다.
pub async fn publish_key_group_share(
    node: &Arc<Node>,
    space: SpaceId,
    group: yangpa_core::ids::KeyGroupId,
) -> Result<SignedEvent> {
    let state = node
        .space_state(&space)
        .ok_or_else(|| Error::not_found("상태 없음"))?;
    if !state.has_key_group(&space, &group) {
        return Err(Error::not_found("그 키 그룹의 채널 없음"));
    }
    let mut rcpts = Vec::new();
    for (user, devices) in state.devices.iter() {
        if !state.can_view_key_group(&space, &group, user) {
            continue;
        }
        for d in devices.active() {
            if let Some(k) = crate::devkeys::device_box_key(node, &state, user, &d.device) {
                rcpts.push((d.device, k));
            }
        }
    }
    let body = EventBody::KeyShare(KeyShareBody {
        keys: vec![{
            let key = Secret32::random();
            SharedKey::Root {
                group,
                id: yangpa_core::body::group_root_id(&space, &group, &key),
                key,
            }
        }],
        for_user: None,
    });
    let l = ingest::lock_space(node, space).await;
    let ev = author::publish_scoped_locked(
        node,
        &l,
        space,
        yangpa_core::event::Scope::KeyGroup(group),
        &body,
        KeyChoice::Sealed(rcpts),
        &[],
    )?;
    drop(l);
    crate::sync_service::propagate(node, space, vec![ev.clone()]);
    Ok(ev)
}

/// 친구 수락을 보내지 않고 붙잡거나(`true`), 풀어 남은 수락을 곧바로 보냅니다.
pub fn hold_friend_accepts(node: &Arc<Node>, hold: bool) {
    crate::friends::hold_accepts(node, hold);
}

/// 그 1:1 공간의 남은 친구 수락으로 보낼 요청과 받을 피어(같은 수락을 겹쳐 보내는 시험).
pub fn pending_accept_request(node: &Node, space: SpaceId) -> Option<(PeerId, YpRequest)> {
    crate::friends::pending_accept_request(node, &space)
}

/// 대기열의 친구 요청을 모두 보내는 중으로 붙잡습니다(보내는 도중 링크 주인과 연결되는 경쟁을 흉내). 놓으면 그사이
/// 다시 보내려던 요청을 곧바로 보냅니다.
pub fn hold_friend_sends(node: &Arc<Node>) -> HeldFriendSends {
    HeldFriendSends {
        _held: crate::friends::hold_pending_redeems(node),
    }
}

/// [`hold_friend_sends`]가 붙잡은 표식(놓으면 풀림).
pub struct HeldFriendSends {
    _held: Vec<crate::friends::Sending>,
}

/// 보내는 중이라 건너뛴 친구 요청 다시 보내기가 있는지.
pub fn friend_resend_wanted(node: &Node) -> bool {
    crate::friends::resend_wanted(node)
}

/// 내가 보낸 친구 요청 수.
pub fn outgoing_requests(node: &Node) -> usize {
    node.store
        .list_friend_requests(yangpa_store::RequestDirection::Outgoing)
        .map(|v| v.len())
        .unwrap_or(0)
}

/// 임시 연락처 만료를 바로 확인합니다(주기를 기다리지 않게).
pub fn expire_contacts_now(node: &Node) {
    crate::friends::expire_due(node);
}

/// 임시 연락처 하나의 만료 시각을 지금으로 당기고 바로 만료합니다(만료를 기다리지 않는 시험).
pub fn expire_contact_now(node: &Node, user: &UserId) {
    if let Ok(Some(f)) = node.store.get_friend(user) {
        let _ = node.store.set_contact_kind(
            user,
            f.kind,
            Some(Timestamp(Timestamp::now().0.saturating_sub(1))),
            f.link_hash,
        );
    }
    crate::friends::expire_due(node);
}

/// 보낸 요청을 남기지 않고 연락처 링크를 씁니다(요청을 잃은 쪽): 링크 주인이 보낸 수락에 모르는 요청이라고 답합니다.
pub async fn redeem_unrecorded(node: &Node, url: &str) -> Result<YpResponse> {
    let link = yangpa_core::links::FriendLink::parse(url)
        .map_err(|e| Error::invalid(format!("링크: {e}")))?;
    let peer = PeerId::from_bytes(&link.contact.peer_id)
        .map_err(|_| Error::invalid("링크의 노드 ID가 잘못됨"))?;
    let me = node.me()?;
    let auth = PeerAuth::create(
        &me.device.signing,
        me.devices.clone(),
        &node.peer_id().to_bytes(),
        &peer.to_bytes(),
    );
    let req = YpRequest::RedeemFriendLink(Box::new(yangpa_core::wire::FriendRedeem {
        token: link.token,
        auth,
        profile: me.profile.clone(),
    }));
    Ok(node.net.request(peer, req).await?)
}

/// 놓친 가입 확인(`CheckJoin`)을 그대로 보냅니다(지목한 크루를 거르는지 확인용).
pub async fn check_join(node: &Node, peer: PeerId, spaces: Vec<SpaceId>) -> Result<YpResponse> {
    let me = node.me()?;
    let auth = PeerAuth::create(
        &me.device.signing,
        me.devices.clone(),
        &node.peer_id().to_bytes(),
        &peer.to_bytes(),
    );
    let req = YpRequest::CheckJoin {
        spaces,
        auth: Box::new(auth),
    };
    Ok(node.net.request(peer, req).await?)
}

/// 예전 노드로 기억한 PeerId가 묶인 기기(`peer.prev.<PeerId>`).
pub fn prev_peer_device(node: &Node, peer: &PeerId) -> Option<DeviceId> {
    crate::peers::prev_peer_device(node, peer)
}

/// 노드에 기록된 주소.
pub fn peer_addrs(node: &Node, peer: &PeerId) -> Vec<String> {
    node.store
        .get_peer(&peer.to_bytes())
        .ok()
        .flatten()
        .map(|r| r.addrs)
        .unwrap_or_default()
}

/// 이 기기의 노드 카드(친구·내 기기에게 주는 연락처).
pub async fn node_contacts(node: &Node) -> Vec<yangpa_core::wire::NodeContact> {
    crate::tor::node_contacts(node).await
}

/// 규모 측정용 상태 색인(`yangpa-bench scale --store`). 노드 없이 색인만 따로 씁니다.
#[derive(Default)]
pub struct IndexProbe(crate::state_index::StateIndexes);

impl IndexProbe {
    /// 전파 한 번이 색인에 묻는 것(활성 기기 수와 목록, 채널마다 누가 보나)과 받기 쪽 멤버 기기 확인 하나. 부분마다
    /// 걸린 시간을 돌려줍니다: [기기 수·목록, 채널(순서대로 합), 기기 → 사용자].
    pub fn touch(
        &self,
        space: &SpaceId,
        state: &Arc<yangpa_core::model::SpaceState>,
        channels: &[ChannelId],
        probe: (&UserId, &DeviceId),
    ) -> [std::time::Duration; 3] {
        let i = self.0.get(space, state);
        let t = std::time::Instant::now();
        std::hint::black_box(i.active_count() + i.devices().len());
        let devices = t.elapsed();
        let t = std::time::Instant::now();
        for c in channels {
            std::hint::black_box(
                i.channel(c, crate::sync_service::DIRECT_VIEWERS_MAX + 1)
                    .viewer_devices,
            );
        }
        let views = t.elapsed();
        let t = std::time::Instant::now();
        std::hint::black_box(i.is_member_device(probe.0, probe.1));
        [devices, views, t.elapsed()]
    }
}

// ───────────── 키 그룹 들여다보기(키 갱신 3부 시험) ─────────────

/// 키 갱신 종류 이름(시험 비교용).
fn kind_name(k: &yangpa_core::keyupdate::KeyUpdateKind) -> &'static str {
    use yangpa_core::keyupdate::KeyUpdateKind as K;
    match k {
        K::Advance { .. } => "advance",
        K::Rotate { .. } => "rotate",
        K::Refresh => "refresh",
        K::FullRekey { .. } => "full_rekey",
        K::RRefresh => "r_refresh",
    }
}

/// 저장된 키 갱신 조각을 상태 키로 풉니다(크루 갱신은 갱신 전 크루 상태 키, 그룹 갱신은 기대는 크루 상태 키).
fn key_update_payload(
    node: &Node,
    space: &SpaceId,
    ev: &SignedEvent,
) -> Option<yangpa_core::keyupdate::KeyUpdatePayload> {
    let (Some(bytes), KeyRef::State(kid)) = (ev.body.as_deref(), ev.header.key) else {
        return None;
    };
    let key = keys::crew_state_key(node, space, &kid).ok()??;
    match yangpa_crypto::decrypt_body(&key, &ev.header, bytes).ok()? {
        EventBody::KeyUpdate(p) => Some(p),
        _ => None,
    }
}

/// 받아들인 키 갱신의 (순번, 종류) 목록(순번 순). `channel`이 없거나 그 채널이 크루 키를 쓰면 크루 사슬(해소기),
/// 아니면 그 채널이 쓰는 그룹 사슬의 이긴 갱신입니다. 종류: `advance`·`rotate`·`refresh`·`full_rekey`·`r_refresh`.
pub fn key_update_kinds(
    node: &Node,
    space: &SpaceId,
    channel: Option<ChannelId>,
) -> Vec<(u64, &'static str)> {
    let group = channel.and_then(|c| {
        let state = node.space_state(space)?;
        crate::keygroup::channel_group(node, space, &state, &c)
    });
    let Some(core) = node.spaces.read().get(space).map(|rt| rt.core.clone()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    match group {
        None => {
            // 받아들인 것은 마지막 조각이라 (작성 기기, 순번, salt)로 머리 조각의 종류를 찾습니다.
            let accepted: std::collections::HashSet<EventId> =
                core.lock().resolver.key_events().iter().copied().collect();
            let mut heads = std::collections::BTreeMap::new();
            let mut ok = std::collections::BTreeSet::new();
            for ev in node
                .store
                .events_by_lane(space, &[Lane::KeyAgreement])
                .unwrap_or_default()
            {
                if ev.header.scope.is_some() {
                    continue;
                }
                let Some(p) = key_update_payload(node, space, &ev) else {
                    continue;
                };
                let k = (ev.header.author, p.seq, p.salt);
                if accepted.contains(&ev.id()) {
                    ok.insert(k);
                }
                if p.part == 0 {
                    heads.insert(k, kind_name(&p.kind));
                }
            }
            for (k, kind) in heads {
                if ok.contains(&k) {
                    out.push((k.1, kind));
                }
            }
        }
        Some(g) => {
            let winners: Vec<EventId> = {
                let mut c = core.lock();
                c.keys.ensure_loaded(node, space);
                c.keys
                    .chains
                    .get(&g)
                    .map(|ch| ch.winners().to_vec())
                    .unwrap_or_default()
            };
            for id in winners {
                let Ok(Some(ev)) = node.store.get_event(&id) else {
                    continue;
                };
                if let Some(p) = key_update_payload(node, space, &ev) {
                    out.push((p.seq, kind_name(&p.kind)));
                }
            }
        }
    }
    out.sort();
    out
}

/// 해소기가 본 크루 키 사슬 끝의 공개 값.
#[derive(Clone, Debug)]
pub struct CrewTip {
    pub seq: u64,
    /// 0이면 직접 인코딩, 아니면 트리 깊이.
    pub depth: u8,
    pub generation: u64,
    pub last_rekey: u64,
    pub members: Vec<DeviceId>,
    /// 표 보유 기기(마지막 전체 재키 뒤의 확정 기기·예비 기기).
    pub holders: Vec<DeviceId>,
    pub rekey_required: bool,
    /// 멤버였는데 활성 기기가 아니게 된, 아직 빼지 않은 기기 수.
    pub debt: usize,
    /// 지금 공간의 활성 기기(크루 그룹에 들어야 할 기기).
    pub active: Vec<DeviceId>,
}

pub fn crew_tip(node: &Node, space: &SpaceId) -> Option<CrewTip> {
    let core = node.spaces.read().get(space).map(|rt| rt.core.clone())?;
    let kv = core.lock().resolver.key_view()?;
    Some(CrewTip {
        seq: kv.crew.seq,
        depth: kv.crew.depth,
        generation: kv.crew.generation,
        last_rekey: kv.crew.last_rekey,
        members: kv.crew.members.iter().copied().collect(),
        holders: kv.crew.holders.iter().copied().collect(),
        rekey_required: kv.crew.rekey_required,
        debt: kv.crew.debt.len(),
        active: kv.owners.keys().copied().collect(),
    })
}

/// 크루 조각 블롭 하나의 크기.
#[derive(Clone, Debug, Default)]
pub struct ShardStats {
    /// 블롭 전체 바이트(머리 조각 + 자료 조각).
    pub blob_bytes: u64,
    pub header_pages: u32,
    pub data_pages: u32,
    pub page_bytes: u32,
    pub users: usize,
    /// 사용자 하나 몫(postcard (사용자, 묶음들))의 평균·최대 바이트.
    pub user_bytes_avg: f64,
    pub user_bytes_max: usize,
}

/// 이 공간의 마지막 크루 조각 블롭 ID(`GetKeyShard`로 물을 때).
pub fn last_shard_blob(node: &Node, space: &SpaceId) -> Option<yangpa_core::ids::BlobId> {
    crate::keygroup::shards::last_blob(node, space)
}

/// `GetKeyShard` 답(`Chunk`의 자료)에 든 묶음의 기기. 묶음으로 읽을 수 없으면 `None`.
pub fn shard_answer_devices(data: &[u8]) -> Option<Vec<DeviceId>> {
    let list: Vec<Vec<u8>> = postcard::from_bytes(data).ok()?;
    list.iter()
        .map(|b| {
            yangpa_crypto::DeviceBundle::decode(b)
                .ok()
                .map(|d| d.device)
        })
        .collect()
}

/// 크루 조각 블롭(트리 전체 재키의 기기 몫)의 마지막 것 크기.
pub fn shard_stats(node: &Node, space: &SpaceId) -> Option<ShardStats> {
    let s = crate::keygroup::shards::stats(node, space)?;
    Some(ShardStats {
        blob_bytes: s.blob_bytes,
        header_pages: s.header_pages,
        data_pages: s.data_pages,
        page_bytes: s.page_bytes,
        users: s.users,
        user_bytes_avg: s.user_bytes_avg,
        user_bytes_max: s.user_bytes_max,
    })
}

/// 논리 시계 `from` 이상인 스트림 이벤트(최대 `limit`개, 논리 시계 순서).
pub fn stream_events_since(
    node: &Node,
    space: &SpaceId,
    stream: StreamId,
    from: u64,
    limit: u32,
) -> Result<Vec<SignedEvent>> {
    let range = LamportRange {
        start: from,
        end: u64::MAX,
    };
    Ok(node.store.stream_events(space, stream, range, limit)?)
}

/// 이 기기가 확정 기기 표를 가졌는지.
pub fn has_key_table(node: &Node, space: &SpaceId) -> bool {
    crate::keygroup::committer::load_table(node, space).is_some()
}

/// 저장된 확정 기기 표 행: (그룹, 저장 칸 0·1, 바이트). 크루가 맨 앞.
pub fn stored_key_rows(
    node: &Node,
    space: &SpaceId,
) -> Vec<(yangpa_core::ids::KeyGroupId, u8, usize)> {
    crate::keygroup::committer::stored_rows(node, space)
}

/// 확정 기기가 크루의 다음 상태(R 새로 하기)를 다른 칸에 써 두고 내기 전에 멈춘 것을 흉내 냅니다. 써 둔 순번.
pub fn stage_unpublished_crew(node: &Node, space: &SpaceId) -> Option<u64> {
    crate::keygroup::committer::stage_unpublished(node, space)
}

/// 확정 기기의 메모리 표가 크루 사슬 끝보다 앞선 것(낸 크루 갱신이 사슬에 들지 못함)을 흉내 냅니다. 앞선 순번.
pub fn stage_crew_ahead(node: &Node, space: &SpaceId) -> Option<u64> {
    crate::keygroup::committer::stage_ahead(node, space)
}

/// 확정 기기 표를 그대로 복사합니다(시험 메모리에만): 자격을 잃은 옛 확정 기기가 몰래 남겨 둔 표를 흉내 냅니다.
pub fn key_table_snapshot(node: &Node, space: &SpaceId) -> Option<zeroize::Zeroizing<Vec<u8>>> {
    crate::keygroup::committer::load_table(node, space).map(|t| t.encode())
}

/// 키 지문: BLAKE3(T ‖ R). 시험이 키 자체를 들고 다니지 않고 같은 키인지만 봅니다.
fn key_digest(t: &Secret32, r: &Secret32) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&t.0);
    h.update(&r.0);
    *h.finalize().as_bytes()
}

/// 표 사본(`key_table_snapshot`, 이 노드가 만든 것)의 그룹마다 (순번, 키 지문).
pub fn key_table_digests(
    node: &Node,
    space: &SpaceId,
    snapshot: &[u8],
) -> Option<std::collections::BTreeMap<yangpa_core::ids::KeyGroupId, (u64, [u8; 32])>> {
    let me = node.me().ok()?;
    let t = crate::keygroup::committer::Table::decode(space, snapshot, &me.device_id())?;
    let mut out = std::collections::BTreeMap::new();
    for tree in t.trees() {
        out.insert(*tree.group(), (tree.seq(), key_digest(tree.t(), tree.r())));
    }
    for c in t.cover_list() {
        out.insert(*c.group(), (c.seq(), key_digest(c.t(), c.r())));
    }
    Some(out)
}

/// 이 기기가 멤버로 가진 지금 그룹 키의 (그룹, 순번, 키 지문): 그 채널이 쓰는 그룹(없거나 전원이 보면 크루).
pub fn member_key_digest(
    node: &Node,
    space: &SpaceId,
    channel: Option<ChannelId>,
) -> Option<(yangpa_core::ids::KeyGroupId, u64, [u8; 32])> {
    let group = channel.and_then(|c| {
        let state = node.space_state(space)?;
        crate::keygroup::channel_group(node, space, &state, &c)
    });
    with_member_keys(node, space, |k, r| {
        let m = match &group {
            None => k.crew_current(space, r)?,
            Some(g) => k.group_current(g)?,
        };
        Some((*m.group(), m.seq(), key_digest(m.t(), m.r())))
    })
}

/// 이 기기가 가진 그룹 상태 전부(순번, 키 요약): 사슬 끝을 따라가지 못한 옛 상태까지 봅니다. `channel`이 `None`이면 크루.
pub fn held_key_digests(
    node: &Node,
    space: &SpaceId,
    channel: Option<ChannelId>,
) -> Vec<(u64, [u8; 32])> {
    let group = match channel {
        None => yangpa_core::ids::KeyGroupId::crew(space),
        Some(c) => {
            let Some(state) = node.space_state(space) else {
                return Vec::new();
            };
            let Some(g) = crate::keygroup::channel_group(node, space, &state, &c) else {
                return Vec::new();
            };
            g
        }
    };
    with_member_keys(node, space, |k, _| {
        let mut v: Vec<_> = k
            .groups
            .get(&group)?
            .states()
            .map(|m| (m.seq(), key_digest(m.t(), m.r())))
            .collect();
        v.sort();
        Some(v)
    })
    .unwrap_or_default()
}

/// 메시지 머리의 채널 뿌리(ID, 키): 이 기기가 저장했거나 보관한 그룹 상태에서 파생할 수 있는 것.
pub fn record_root_of(node: &Node, header: &EventHeader) -> Option<(ChannelRootId, Secret32)> {
    let (KeyRef::Record { root }, Some(channel)) = (header.key, header.channel) else {
        return None;
    };
    if let Ok(Some(k)) = keys::root(node, &root) {
        return Some((root, k));
    }
    let space = header.space;
    let state = node.space_state(&space)?;
    let group = crate::keygroup::channel_group(node, &space, &state, &channel);
    with_member_keys(node, &space, |k, _| {
        k.derive_root(&space, &channel, group.as_ref(), &root)
    })
    .map(|k| (root, k))
}

/// 답하는 멤버처럼 채널 뿌리들을 요청 기기(`user`의 `device`)의 상자 키로 봉인한 `SealedKeys` 바이트를 만듭니다. 키가
/// 맞는지는 보지 않으므로 틀린 키를 보내는 멤버를 흉내 낼 수 있습니다.
pub fn seal_relayed_roots(
    responder: &Node,
    space: &SpaceId,
    user: &UserId,
    device: &DeviceId,
    roots: Vec<(ChannelId, Secret32)>,
) -> Option<Vec<u8>> {
    crate::keygroup::relay::seal_roots_for(responder, space, user, device, roots)
}

/// 뿌리 ID들을 물은 것처럼 중계 답을 받아들입니다(실제 묻기와 같은 열기·ID 확인). 반환: 저장한 뿌리 수.
pub async fn accept_relayed_roots(
    node: &Arc<Node>,
    space: SpaceId,
    channel: ChannelId,
    ids: Vec<ChannelRootId>,
    sealed: &[u8],
) -> usize {
    crate::keygroup::relay::accept_roots(node, space, channel, ids, sealed).await
}

/// 이 기기의 저장소에 이 채널 뿌리가 있는지(중계·파생으로 얻어 저장한 것).
pub fn has_root(node: &Node, id: &ChannelRootId) -> bool {
    keys::root(node, id).ok().flatten().is_some()
}

/// 놓친 채널 뿌리를 멤버에게 묻지 않게(`true`) 하거나 다시 묻게 합니다(중계 답을 시험이 직접 넣을 때).
pub fn hold_root_relay(node: &Node, hold: bool) {
    node.keygroup
        .relay_hold
        .store(hold, std::sync::atomic::Ordering::Relaxed);
}

/// 저장소를 보지 않고, 이 기기가 보관한 그룹 상태·받은 R만으로 그 메시지의 채널 뿌리를 파생할 수 있는지.
pub fn can_derive_root(node: &Node, header: &EventHeader) -> bool {
    let (KeyRef::Record { root }, Some(channel)) = (header.key, header.channel) else {
        return false;
    };
    let space = header.space;
    let Some(state) = node.space_state(&space) else {
        return false;
    };
    let group = crate::keygroup::channel_group(node, &space, &state, &channel);
    with_member_keys(node, &space, |k, _| {
        k.derive_root(&space, &channel, group.as_ref(), &root)
    })
    .is_some()
}

/// 초대 링크로 들어온 것처럼 지어낸 사용자 `users`명(사용자마다 기기 `devices_each`대)의 `MemberJoin`을 이 노드의
/// 기기로 서명하고 이 노드에 적용합니다(전파하지 않음, 돌려준 이벤트를 다른 노드에 넣으세요). 첫 기기만 진짜 서명 키
/// (목록 공동 서명)이고 나머지는 서명할 수 없는 키라 빠릅니다. 상자 키는 아무도 열 수 없는 값입니다: 키 그룹이
/// 직접 인코딩 상한(256대)을 넘는 크루를 시험 노드 몇 개로 만들 때 씁니다.
pub async fn synthetic_members(
    node: &Arc<Node>,
    space: SpaceId,
    token: &LinkToken,
    users: usize,
    devices_each: usize,
) -> Result<Vec<SignedEvent>> {
    let mut out = Vec::with_capacity(users);
    for _ in 0..users {
        let identity = yangpa_core::sign::SigningKeypair::generate();
        let first = yangpa_core::sign::SigningKeypair::generate();
        let user = UserId(identity.public());
        let now = Timestamp::now();
        let mut devices = vec![DeviceEntry {
            device: DeviceId(first.public()),
            box_key: BoxPublicKey::filled(3),
            sign_keys: DeviceSignKeys::of(&first),
            name: String::new(),
            added_at: now,
            revoked_at: None,
        }];
        for _ in 1..devices_each {
            let sign_keys = fake_sign_keys();
            devices.push(DeviceEntry {
                device: sign_keys.device_id(),
                box_key: BoxPublicKey::filled(3),
                sign_keys,
                name: String::new(),
                added_at: now,
                revoked_at: None,
            });
        }
        let list = DeviceList {
            user,
            version: now.0,
            devices,
            otp: None,
        }
        .sign_with(&identity, &first)?;
        let proof = InviteProof::create(token, &space, &list, now)?
            .signed_by_joiner(&space, &list, &first)?;
        let body = EventBody::MemberJoin {
            member: Member {
                user,
                roles: Vec::new(),
                nickname: None,
                automation: false,
            },
            devices: list,
            profile: None,
            invite: Some(proof),
        };
        out.push(author::author_event(
            node,
            &space,
            None,
            &body,
            &KeyChoice::State,
            &[],
        )?);
    }
    ingest::ingest(node, space, out.clone(), Source::Local).await?;
    crate::keygroup::on_state_changed(node, space);
    Ok(out)
}
