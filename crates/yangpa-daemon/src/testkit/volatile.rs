//! 휘발성 채널(B2) 시험 도우미: 여러 사람 공간 만들기, 설정을 고른 채널, 휘발성 RAM 상태 들여다보기.
//!
//! 들여다보기는 세대 번호·암호문만 돌려주고 세션 키·평문은 내보내지 않습니다.

use std::sync::Arc;

use yangpa_core::body::{EventBody, Secret32};
use yangpa_core::event::{EventKind, SignedEvent};
use yangpa_core::ids::{BlobId, ChannelId, DeviceId, SpaceId, UserId};
use yangpa_core::model::{ChannelSettings, Member, Permissions, SpaceKind, SpaceSettings};
use yangpa_core::wire::YpResponse;
use yangpa_net::PeerId;

use crate::author::{self, KeyChoice};
use crate::error::{Error, Result};
use crate::ingest::{self, Source};
use crate::node::Node;
use crate::spaces::{self, NewSpace};
use crate::{sync_service, volatile};

fn member(user: UserId) -> Member {
    Member {
        user,
        roles: Vec::new(),
        nickname: None,
        automation: false,
    }
}

/// `owner`가 `others`를 처음 멤버로 둔 크루를 만들고, 만든 공간 스트림 이벤트를 다른 노드에도 적용합니다
/// (크루 초대 흐름 전의 시험용). 노드끼리는 따로 연결·인증해야 합니다.
pub async fn create_group_space(owner: &Arc<Node>, others: &[Arc<Node>]) -> Result<SpaceId> {
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
            name: "시험 크루".into(),
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

/// 설정을 그대로 써서 채널을 만들어 적용·전파합니다.
pub async fn create_channel_with(
    node: &Arc<Node>,
    space: SpaceId,
    settings: ChannelSettings,
) -> Result<ChannelId> {
    let channel = ChannelId::random();
    let body = EventBody::ChannelCreate { channel, settings };
    author::publish(node, space, None, &body, KeyChoice::State).await?;
    Ok(channel)
}

/// 이 노드가 세션 키를 가진 세대.
pub fn volatile_generations(node: &Node, space: &SpaceId, channel: &ChannelId) -> Vec<u64> {
    volatile::inspect::generations(node, space, channel)
}

/// 보낼 때 쓰는 세대.
pub fn volatile_current_generation(
    node: &Node,
    space: &SpaceId,
    channel: &ChannelId,
) -> Option<u64> {
    volatile::inspect::current_generation(node, space, channel)
}

/// RAM에 있는 휘발성 이벤트(암호문 그대로).
pub fn volatile_raw_events(node: &Node, space: &SpaceId, channel: &ChannelId) -> Vec<SignedEvent> {
    volatile::inspect::raw_events(node, space, channel)
}

/// 이 채널의 세션 키가 하나라도 남았는지.
pub fn volatile_has_key(node: &Node, space: &SpaceId, channel: &ChannelId) -> bool {
    volatile::inspect::has_any_key(node, space, channel)
}

/// 다음 `n`번의 `peer`로 보내는 입장 알림(`VolatilePresence(true)`)을 실패로 처리합니다(알림 유실 흉내).
pub fn fail_presence_to(node: &Node, peer: PeerId, n: u32) {
    volatile::inspect::fail_presence_to(node, peer, n);
}

/// 다음 `n`번의 `peer`로 보내는 세션 키 갱신을 실패로 처리합니다(갱신 전달 유실 흉내).
pub fn fail_key_to(node: &Node, peer: PeerId, n: u32) {
    volatile::inspect::fail_key_to(node, peer, n);
}

/// `peer`로 보내는 세션 키 갱신 중 아직 실패로 처리할 남은 횟수(0이면 걸어 둔 실패를 모두 겪음).
pub fn key_faults_left(node: &Node, peer: PeerId) -> u32 {
    volatile::inspect::key_faults_left(node, peer)
}

/// 다음 `n`번의 스냅숏 받아 오기를 건너뜁니다(늦은 입장자의 받아 오기 실패 흉내).
pub fn fail_snapshot(node: &Node, n: u32) {
    volatile::inspect::fail_snapshot(node, n);
}

/// 지금 보내는 세션 키로 휘발성 메시지를 만들어 서명만 합니다(적용·전송하지 않음, 암호문만 돌려줌).
/// `lamport`를 주면 그 논리 시계를 씁니다.
pub fn author_unsent(
    node: &Node,
    space: SpaceId,
    channel: ChannelId,
    text: &str,
    lamport: Option<u64>,
) -> Result<SignedEvent> {
    volatile::inspect::author_unsent(node, space, channel, text, lamport)
}

/// `from` 피어가 `VolatileSnapshot`에 이 항목들(이벤트, 남은 수명 ms)로 답한 것처럼 적용합니다.
pub fn deliver_snapshot(
    node: &Arc<Node>,
    space: SpaceId,
    channel: ChannelId,
    from: PeerId,
    items: Vec<(SignedEvent, Option<u64>)>,
) -> Result<()> {
    volatile::inspect::deliver_snapshot(node, space, channel, from, items)
}

/// 이 노드가 지금까지 받아 둔 휘발성 세션 키 감싸기 수.
pub fn wraps_received(node: &Node) -> u64 {
    volatile::inspect::wraps_received(node)
}

/// 그 세대 세션 키의 약속값(기기끼리 같은 키를 가졌는지 비교용). 키가 없으면 `None`.
pub fn key_commit(
    node: &Node,
    space: &SpaceId,
    channel: &ChannelId,
    generation: u64,
) -> Option<[u8; 32]> {
    volatile::inspect::key_commit(node, space, channel, generation)
}

/// 이 노드와 `to` 둘만 받는 회전 공지(약속은 `commit_key`)를 만듭니다. (공지, 세대, 받는 기기 목록).
pub fn forge_rotation(
    node: &Node,
    space: SpaceId,
    channel: ChannelId,
    to: DeviceId,
    commit_key: &Secret32,
) -> Result<(SignedEvent, u64, Vec<DeviceId>)> {
    volatile::inspect::forge_rotation(node, space, channel, to, commit_key)
}

/// `to`에게 `wrap_key`를 봉인한 그 세대 감싸기(약속은 `commit_key`). 둘이 다르면 거짓 감싸기.
pub fn forge_wrap(
    node: &Node,
    space: SpaceId,
    channel: ChannelId,
    generation: u64,
    to: DeviceId,
    commit_key: &Secret32,
    wrap_key: &Secret32,
) -> Result<Vec<u8>> {
    volatile::inspect::forge_wrap(node, space, channel, generation, to, commit_key, wrap_key)
}

/// `from` 피어가 `VolatileKeyWrap`을 보낸 것처럼 처리하고 응답을 돌려줍니다.
pub fn deliver_wrap(
    node: &Arc<Node>,
    from: PeerId,
    space: SpaceId,
    channel: ChannelId,
    generation: u64,
    wrap: Vec<u8>,
    recipients: Option<Vec<DeviceId>>,
) -> YpResponse {
    volatile::inspect::deliver_wrap(node, from, space, channel, generation, wrap, recipients)
}

/// 이 노드 저장소의 공간 논리 시계 최댓값.
pub fn max_lamport(node: &Node, space: &SpaceId) -> Result<u64> {
    Ok(node.store.max_lamport(space)?)
}

/// 이 노드 RAM 저장소의 휘발성 첨부 상태(주인 메시지, 받은 조각 수, 암호문 크기). 없으면 `None`.
pub fn volatile_blob_info(
    node: &Node,
    space: &SpaceId,
    blob: &BlobId,
) -> Option<yangpa_store::RamBlobInfo> {
    volatile::inspect::blob_info(node, space, blob)
}

/// 이 노드 RAM 저장소 통계(첨부 수·바이트·잡아 둔 예산 포함, 내용 없음).
pub fn volatile_stats(node: &Node) -> yangpa_store::VolatileStats {
    volatile::stats(node)
}

/// 지금 받는 중인 휘발성 첨부 수.
pub fn volatile_fetching(node: &Node) -> usize {
    volatile::inspect::fetching(node)
}

/// 휘발성 첨부 받기 시험용 조절: 요청 중인 것 없이 진전이 없으면 시도를 끝내는 시간, 진전 없이 그만 받는 시간,
/// 받은 조각 + 요청 중인 조각이 `hold_at`에 닿으면 더 요청하지 않기(보낸 기기가 중간에 나간 경우 흉내).
pub fn tune_volatile_fetch(
    node: &Node,
    stall: Option<std::time::Duration>,
    give_up: Option<std::time::Duration>,
    hold_at: Option<u32>,
) {
    volatile::inspect::tune_fetch(
        node,
        volatile::attach::Tuning {
            stall,
            give_up,
            hold_at,
        },
    );
}

/// 다음 `n`번의 휘발성 첨부를 메시지에 묶지 못한 것으로 처리합니다(묶기 전에 첨부가 사라진 경우 흉내).
pub fn fail_volatile_claims(node: &Node, n: u32) {
    volatile::inspect::fail_claims(node, n);
}

/// 내려받기 자리: (일반 첨부 내려받기 수, 휘발성 첨부에 빌려준 자리 수, 자리를 기다리는 휘발성 첨부가 있는지).
pub fn download_slots(node: &Node) -> (usize, usize, bool) {
    crate::blobs::slot_stats(node)
}
