//! 음성·Tor·위치 기록·주소 기록(B5) 시험 도우미.
//!
//! 통화 들여다보기는 세대와 키 지문만 돌려주고 키 자체는 내보내지 않습니다.

use std::sync::Arc;

use rand::RngCore;
use yangpa_core::body::EventBody;
use yangpa_core::event::{EventKind, SignedEvent};
use yangpa_core::ids::{CallId, ChannelId, DeviceId, SpaceId, UserId};
use yangpa_core::model::{
    ChannelKind, ChannelSettings, Member, OverwriteTarget, PermissionOverwrite, Permissions,
    SpaceKind, SpaceSettings,
};
use yangpa_core::wire::{GossipMessage, GossipTopic, SignalPayload, YpRequest};
use yangpa_net::{Multiaddr, NetMode, PeerId};

use crate::author::{self, KeyChoice};
use crate::error::{Error, Result};
use crate::ingest::{self, Source};
use crate::node::Node;
use crate::spaces::{self, NewSpace};
use crate::{locator, sync_service, voice};

fn member(user: UserId) -> Member {
    Member {
        user,
        roles: Vec::new(),
        nickname: None,
        automation: false,
    }
}

/// `owner`가 `others`를 처음 멤버로 둔 크루를 만들고, 만든 공간 스트림 이벤트를 다른 노드에도 적용합니다
/// (크루 초대 흐름을 거치지 않는 시험용). 노드끼리는 따로 연결·인증해야 합니다.
pub async fn create_crew(owner: &Arc<Node>, others: &[Arc<Node>]) -> Result<SpaceId> {
    let me = owner.me()?;
    let mut members = vec![member(me.user())];
    let mut devices = vec![me.devices.clone()];
    for o in others {
        let om = o.me()?;
        members.push(member(om.user()));
        devices.push(om.devices.clone());
    }
    let spec = NewSpace {
        kind: SpaceKind::Crew,
        settings: SpaceSettings {
            name: "음성 시험".into(),
            description: String::new(),
            icon: None,
            icon_key: None,
            allow_external_emoji: false,
            strict_rotation: false,
        },
        everyone: Permissions::DEFAULT_EVERYONE,
        members,
        devices,
        first_channel: Some("일반".into()),
    };
    let l = ingest::lock(owner).await;
    let (space, events, report) = spaces::create_space_locked(owner, &l, spec)?;
    drop(l);
    ingest::after_unlock(owner, space, &report);
    sync_service::refresh_space_peers(owner, &space);
    let genesis = events
        .iter()
        .find(|e| e.header.kind == EventKind::SpaceCreate)
        .map(SignedEvent::id)
        .ok_or_else(|| Error::internal("공간 생성 이벤트가 없음"))?;
    for o in others {
        spaces::ensure_runtime(o, &space, Some(genesis))?;
        ingest::ingest(o, space, events.clone(), Source::Bootstrap(owner.peer_id())).await?;
        sync_service::refresh_space_peers(o, &space);
    }
    Ok(space)
}

/// 통화 채널을 만들어 적용·전파합니다.
pub async fn create_voice_channel(
    node: &Arc<Node>,
    space: SpaceId,
    name: &str,
) -> Result<ChannelId> {
    let channel = ChannelId::random();
    let settings = ChannelSettings {
        kind: ChannelKind::Voice,
        ..ChannelSettings::text(name)
    };
    let body = EventBody::ChannelCreate { channel, settings };
    author::publish(node, space, None, &body, KeyChoice::State).await?;
    Ok(channel)
}

/// 이 노드가 보는 통화.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallView {
    pub joined: bool,
    pub devices: Vec<DeviceId>,
    pub generation: u64,
    /// 지금 키의 BLAKE3 지문(앞 8바이트). 기기끼리 같은 키인지 비교할 때 씁니다.
    pub key_fingerprint: Option<[u8; 8]>,
}

pub fn voice_call(node: &Node, space: &SpaceId, channel: &ChannelId) -> Option<CallView> {
    voice::inspect::call(node, space, channel).map(
        |(joined, devices, generation, key_fingerprint)| CallView {
            joined,
            devices,
            generation,
            key_fingerprint,
        },
    )
}

/// 뱃지가 통화 시간을 세는 상태(다른 사람이 있는 통화에 있는지, 내 화면을 공유하는지).
pub fn voice_talking(node: &Node) -> (bool, bool) {
    voice::talking(node)
}

/// `node`의 기기가 `to` 기기에게만 봉인한 임의의 프레임 키 신호(관리 기기가 아닌 참가자가 키를 심으려는 상황).
/// (보낼 요청, 그 키의 지문)을 돌려줍니다.
pub fn forged_media_key(
    node: &Node,
    space: SpaceId,
    channel: ChannelId,
    to: DeviceId,
    generation: u64,
) -> Result<(YpRequest, [u8; 8])> {
    let me = node.me()?;
    let state = node
        .space_state(&space)
        .ok_or_else(|| Error::not_found("모르는 공간"))?;
    let box_key = state
        .active_devices()
        .find(|(_, d)| d.device == to)
        .and_then(|(u, _)| crate::devkeys::device_box_key(node, &state, u, &to))
        .ok_or_else(|| Error::not_found("모르는 기기"))?;
    let mut key = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut key);
    let sealed = yangpa_crypto::seal_to(
        &[(to, box_key)],
        &voice::media_ad(&space, &channel, generation, &me.device_id()),
        &key,
    );
    let req = YpRequest::Signal {
        space,
        channel,
        call: CallId::random(),
        from: me.device_id(),
        payload: SignalPayload::MediaKey { generation, sealed },
    };
    Ok((req, voice::inspect::fingerprint(&key)))
}

/// 채널에 멤버 한 명의 권한 덮어쓰기(`deny`)를 더하는 상태 이벤트를 냅니다(소유자 노드에서).
pub async fn deny_in_channel(
    node: &Arc<Node>,
    space: SpaceId,
    channel: ChannelId,
    user: UserId,
    deny: Permissions,
) -> Result<()> {
    overwrite_member(node, space, channel, user, Permissions::NONE, deny).await
}

/// 채널에 멤버 한 명의 권한 덮어쓰기(`allow`)를 더하는 상태 이벤트를 냅니다(소유자 노드에서).
pub async fn allow_in_channel(
    node: &Arc<Node>,
    space: SpaceId,
    channel: ChannelId,
    user: UserId,
    allow: Permissions,
) -> Result<()> {
    overwrite_member(node, space, channel, user, allow, Permissions::NONE).await
}

async fn overwrite_member(
    node: &Arc<Node>,
    space: SpaceId,
    channel: ChannelId,
    user: UserId,
    allow: Permissions,
    deny: Permissions,
) -> Result<()> {
    let state = node
        .space_state(&space)
        .ok_or_else(|| Error::not_found("모르는 공간"))?;
    let mut settings = state
        .channels
        .get(&channel)
        .ok_or_else(|| Error::not_found("모르는 채널"))?
        .clone();
    settings.overwrites.push(PermissionOverwrite {
        target: OverwriteTarget::Member(user),
        allow,
        deny,
    });
    let body = EventBody::ChannelUpdate { channel, settings };
    author::publish(node, space, None, &body, KeyChoice::State).await?;
    Ok(())
}

/// `from` 기기로 서명한 오늘 위치 기록(`into`의 지금 스웜 종류, `peer`·`addrs`)을 `into`의 DHT 로컬 저장소에
/// 넣고 `into`의 찾기 캐시에서 그 기기를 지웁니다. DHT 이웃이 없으면 퍼뜨리기는 실패하지만 로컬 저장소에는 남아
/// 이후 찾기(`locator::lookup`)가 이 기록을 돌려줍니다(미끼 주소가 든 기록을 받은 상황 흉내).
pub async fn plant_locator_record(
    from: &Node,
    into: &Node,
    peer: PeerId,
    addrs: &[Multiaddr],
) -> Result<()> {
    let me = from.me()?;
    let device = me.device_id();
    let day = locator::today();
    let mode = into.net.last_status().mode;
    let value = locator::seal_record(me.user(), &me.device, day, mode, &peer, addrs)?;
    // 이웃이 없으면 정족수 실패로 끝나지만 Kademlia는 먼저 로컬 저장소에 넣습니다.
    let _ = into
        .net
        .put_record(locator::record_key(&device, day, mode), value)
        .await;
    locator::forget_cached(into, &device);
    Ok(())
}

/// 받은 gossip 메시지(`events`, `from`이 전달)를 적용하고 네트워크 계층에 알릴 판정을 돌려줍니다.
pub async fn gossip_verdict(
    node: &Arc<Node>,
    from: PeerId,
    space: SpaceId,
    events: Vec<SignedEvent>,
) -> yangpa_net::GossipVerdict {
    gossip_verdict_on(node, from, GossipTopic::Crew(space), events).await
}

/// [`gossip_verdict`]를 `topic`(그룹 토픽 등)으로 받았을 때.
pub async fn gossip_verdict_on(
    node: &Arc<Node>,
    from: PeerId,
    topic: GossipTopic,
    events: Vec<SignedEvent>,
) -> yangpa_net::GossipVerdict {
    let space = topic.space();
    sync_service::gossip_verdict(node, from, topic, GossipMessage::Events { space, events }).await
}

/// 마지막으로 올린 위치 기록의 (UTC 일 번호, PeerId).
pub fn locator_published(node: &Node) -> Option<(u32, PeerId)> {
    locator::published(node)
}

/// 오늘 날짜 위치 기록의 DHT 키(기기 ID를 아는 사람만 만들 수 있음). 스웜 종류마다 다릅니다.
pub fn locator_key(device: &DeviceId, mode: NetMode) -> Vec<u8> {
    locator::record_key(device, locator::today(), mode)
}

/// 직접 연결 스웜(`NetMode::Normal`)이 올린 DHT 값을 기기 ID로 열어 PeerId·주소를 꺼냅니다(못 열면 `None`).
pub fn locator_open(device: &DeviceId, value: &[u8]) -> Option<(PeerId, Vec<Multiaddr>)> {
    locator::open_record(device, locator::today(), NetMode::Normal, value)
        .map(|r| (r.peer, r.addrs))
}

/// 위치 기록으로 기기의 노드를 찾습니다(조회 결과 캐시 없이 DHT에서).
pub async fn locator_find(node: &Node, device: &DeviceId) -> Option<PeerId> {
    locator::find_record(node, device).await.map(|r| r.peer)
}

/// 낡은 주소 대신 쓰는 조회: 위치 기록 → PeerId → (주소가 없으면) Kademlia로 주소. 결과는 잠깐 캐시됩니다.
pub async fn locator_lookup(
    node: &Arc<Node>,
    user: &UserId,
    device: &DeviceId,
) -> Option<(PeerId, Vec<Multiaddr>)> {
    locator::lookup(node, user, device).await
}

/// 기록된 노드로 보내기가 실패해 위치 기록으로 찾은 노드에 이벤트를 보낸 횟수.
pub fn push_reroutes(node: &Node) -> u64 {
    locator::reroutes(node)
}

/// 다음 시작부터 쓸 직접 연결 네트워크 키를 새로 만듭니다(PeerId가 바뀜. 예전 빌드에서 업그레이드해 PeerId가 바뀐 상황).
pub fn rotate_net_key(node: &Node) -> Result<()> {
    let seed = yangpa_core::body::Secret32::random();
    Ok(node
        .store
        .put_secret(yangpa_store::secret_names::NET_KEY, &seed.0)?)
}

/// DHT 값을 그대로 가져옵니다(키만 아는 노드가 보는 것).
pub async fn dht_get(node: &Node, key: Vec<u8>) -> Vec<Vec<u8>> {
    node.net.get_record(key).await.unwrap_or_default()
}

/// Kademlia로 노드 주소를 찾습니다.
pub async fn dht_find_peer(node: &Node, peer: PeerId) -> Vec<Multiaddr> {
    node.net.find_peer(peer).await.unwrap_or_default()
}

/// 실행 중인 주 스웜에 상시 노드를 더합니다. 사용자 설정(`net.add_bootstrap`)은 없어졌고, 시험에서 노드를 DHT·
/// 릴레이 노드에 잇는 데만 씁니다(저장하지 않음: 다시 띄울 때는 시작 옵션 `cfg.net.bootstrap`으로).
pub fn add_bootstrap(node: &Node, addrs: Vec<Multiaddr>) {
    node.net.add_bootstrap(addrs);
}

/// 예전 빌드가 남긴 설정을 흉내 내 저장합니다(다음 시작의 정리를 시험).
pub fn put_legacy_setting(node: &Node, key: &str, on: bool) -> Result<()> {
    Ok(node.store.put_setting(key, &on)?)
}

/// 설정이 저장돼 있는지.
pub fn has_setting(node: &Node, key: &str) -> bool {
    node.store
        .get_setting::<serde_json::Value>(key)
        .ok()
        .flatten()
        .is_some()
}

/// 직접 연결 네트워크 키(`NET_KEY`)가 남아 있는지.
pub fn has_direct_net_key(node: &Node) -> bool {
    node.store
        .get_secret(yangpa_store::secret_names::NET_KEY)
        .ok()
        .flatten()
        .is_some()
}

/// 노드 주소 기록(`peer_cache`)에 남은 주소.
pub fn cached_addrs(node: &Node, peer: &PeerId) -> Vec<String> {
    node.store
        .get_peer(&peer.to_bytes())
        .ok()
        .flatten()
        .map(|r| r.addrs)
        .unwrap_or_default()
}

/// 인증된 연결이 있는지.
pub fn is_authed(node: &Node, peer: &PeerId) -> bool {
    node.peers.identity_of(peer).is_some()
}

/// 지금 스웜의 실제 구성(전송·켜진 동작·알리는 주소). Tor 누출 시험용.
pub async fn inspect(node: &Node) -> Option<yangpa_net::NetInspection> {
    node.net.inspect().await
}

/// 지금 스웜의 Arti 경유 다이얼·앱 스트림 시도 수.
pub fn tor_activity(node: &Node) -> yangpa_net::TorActivity {
    node.net.tor_activity()
}

/// identify로 들은 주소를 기록하지 않게 합니다(Tor 스웜처럼 주소는 `Social(Contact)`로만 앎).
pub fn skip_identify_addrs(node: &Node) {
    crate::locator::skip_identify_addrs(node);
}

/// 지금 예약한 릴레이(고른 순서).
pub fn relays(node: &Node) -> Vec<yangpa_net::RelayNode> {
    node.net.last_status().relays
}

/// 내 링크용 주소(릴레이 경유·onion).
pub fn shareable_addrs(node: &Node) -> Vec<Multiaddr> {
    node.net.last_status().shareable_addrs()
}
