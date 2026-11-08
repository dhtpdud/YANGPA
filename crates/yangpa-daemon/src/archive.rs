//! 보관 전용 노드(plan.md "선택 기능"): 복호화 키 없이 암호화된 이벤트·첨부만 받아 두고 현재 멤버에게 내줍니다.
//!
//! - 보관 쪽([`host`], `yangpad --archive`): 지정 수락·명단·받아 두기·내주기·삭제 반영·해제.
//! - 멤버 쪽([`member`]): 지정·해제, 명단·내 이벤트·첨부 알림·삭제 반영 보내기, 보관 노드를 시더처럼 쓰기.
//!
//! 다른 모듈에는 작은 고리만 둡니다: `inbound`([`route`]·[`handle_request`]), `peers`([`trusts`]·[`archive_only`]),
//! `sync_service`([`check_seed`]·[`outgoing`]·[`forward`]·[`on_peer_authed`]·[`is_active`]), `blobs`([`archive_peers`]·
//! [`is_active`]), `identity`(프로필 교환 건너뛰기), `api/seeder`([`coverage`]). 설계는 docs/protocol.md "보관 전용 노드".

mod host;
mod member;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use yangpa_core::ids::{BlobId, ChannelId, DeviceId, SpaceId, UserId};
use yangpa_core::wire::{WireErrorCode, YpRequest, YpResponse};
use yangpa_net::PeerId;

use crate::node::Node;

pub(crate) use host::{HostedInfo, OFFER_DEFAULT_TTL};
pub(crate) use member::{
    add_joiner, archive_peers, check_seed, coverage, designate, forward, is_active, list, revoke,
};

/// 주기 작업 최대 간격(동기화 간격이 더 짧으면 그것).
const MAX_TICK: Duration = Duration::from_secs(60);

#[derive(Default)]
pub(crate) struct ArchiveState {
    host: host::HostState,
    member: member::MemberState,
    /// 주기 작업을 바로 돌리게 깨움(지정·명단 변화·삭제·내 메시지).
    wake: Arc<Notify>,
}

pub(crate) fn start(node: &Arc<Node>) {
    host::load(node);
    member::start(node);
    let weak = Arc::downgrade(node);
    let wake = node.archive.wake.clone();
    let interval = node.cfg.sync_interval.min(MAX_TICK);
    node.tasks.spawn(async move {
        loop {
            {
                let Some(node) = weak.upgrade() else { break };
                if node.is_shutting_down() {
                    break;
                }
                member::tick(&node).await;
                host::tick(&node);
            }
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = wake.notified() => {}
            }
        }
    });
}

/// 주기 작업을 곧 다시 돌립니다(깨움이 겹치면 한 번).
pub(crate) fn wake(node: &Node) {
    node.archive.wake.notify_one();
}

/// 인증 고리(`peers::is_trusted`): 내가 멤버인 공간에 지정된 보관 기기이거나, 내가 받아 둔 공간 명단의 멤버 기기.
pub(crate) fn trusts(node: &Node, user: &UserId, device: &DeviceId) -> bool {
    member::trusts(node, user, device) || host::trusts(node, user, device)
}

/// 보관 관계로만 믿는 상대인지(내 기기·친구·함께 있는 공간의 멤버가 아님). 이런 연결에서는 프로필을 주고받지 않고
/// 동기화·블롭·보관 요청만 받습니다.
pub(crate) fn archive_only(node: &Node, user: &UserId, device: &DeviceId) -> bool {
    if !trusts(node, user, device) {
        return false;
    }
    let Ok(me) = node.me() else { return false };
    if me.user() == *user || node.store.get_friend(user).ok().flatten().is_some() {
        return false;
    }
    !node
        .space_ids()
        .iter()
        .any(|s| node.space_state(s).is_some_and(|st| st.is_member(user)))
}

/// 들어온 요청 고리(`inbound::handle`, 인증 확인 뒤): 받아 둔 공간의 동기화·블롭 요청은 보관 쪽이 답하고, 보관
/// 관계로만 믿는 연결에서는 동기화·블롭·보관 요청만 받습니다. 그 밖에는 요청을 돌려줘 보통 처리로 보냅니다.
#[allow(clippy::result_large_err)]
pub(crate) fn route(
    node: &Arc<Node>,
    peer: PeerId,
    req: YpRequest,
) -> Result<YpResponse, YpRequest> {
    if let Some(space) = host::request_space(&req)
        && !node.spaces.read().contains_key(&space)
        && host::is_hosted(node, &space)
    {
        return Ok(host::serve(node, peer, req));
    }
    let Some((user, device)) = node.peers.identity_of(&peer) else {
        return Err(req);
    };
    if !archive_only(node, &user, &device) {
        return Err(req);
    }
    match req {
        YpRequest::Ping
        | YpRequest::Auth(_)
        | YpRequest::Archive(_)
        | YpRequest::GetHeads { .. }
        | YpRequest::GetFingerprints { .. }
        | YpRequest::GetIds { .. }
        | YpRequest::GetEvents { .. }
        | YpRequest::BlobManifest { .. }
        | YpRequest::BlobHave { .. }
        | YpRequest::BlobChunk { .. } => Err(req),
        _ => Ok(YpResponse::error(
            WireErrorCode::Forbidden,
            "보관 전용 연결에서는 받지 않는 요청",
        )),
    }
}

/// `YpRequest::Archive`. 지정·해제는 인증 전에도 받습니다(요청에 연결에 묶인 기기 증명이 실림).
pub(crate) async fn handle_request(
    node: &Arc<Node>,
    peer: PeerId,
    req: yangpa_core::archive::ArchiveRequest,
) -> YpResponse {
    host::handle_request(node, peer, req).await
}

/// 따라잡기 고리(`sync_service::catch_up_peer`): 보관 쪽이면 이 멤버 기기에게서 받아 둔 공간을 당겨옵니다.
pub(crate) fn on_peer_authed(node: &Arc<Node>, peer: PeerId, user: &UserId, device: &DeviceId) {
    host::on_peer_authed(node, peer, user, device);
}

// ───────────── API(보관 쪽) ─────────────

pub(crate) async fn offer(
    node: &Arc<Node>,
    ttl: Option<Duration>,
    name: Option<String>,
) -> crate::error::Result<(String, Option<yangpa_core::time::Timestamp>)> {
    host::offer(node, ttl.unwrap_or(OFFER_DEFAULT_TTL), name).await
}

pub(crate) fn hosted(node: &Node) -> Vec<HostedInfo> {
    host::hosted_info(node)
}

/// `archive.drop`: 받아 둔 공간을 지웁니다(운영자가 직접). 없으면 false.
pub(crate) fn drop_hosted(node: &Node, space: &SpaceId) -> bool {
    host::purge(node, space)
}

// ───────────── 시험용 ─────────────

/// 받아 둔 공간인지.
pub(crate) fn is_hosted(node: &Node, space: &SpaceId) -> bool {
    host::is_hosted(node, space)
}

/// 진행 중인 당겨오기·첨부 받기가 없는지.
pub(crate) fn host_idle(node: &Node) -> bool {
    host::pull_idle(node)
}

/// 받아 둔 공간 명단의 멤버.
pub(crate) fn roster_members(node: &Node, space: &SpaceId) -> Option<Vec<UserId>> {
    host::roster_members(node, space)
}

/// 받아 둔 명단에서 이 멤버가 들어온 논리 시계(멤버가 아니면 `None`).
pub(crate) fn roster_since(node: &Node, space: &SpaceId, user: &UserId) -> Option<Option<u64>> {
    host::roster_since(node, space, user)
}

/// 받아 둔 명단의 보관 채널.
pub(crate) fn roster_channels(node: &Node, space: &SpaceId) -> Option<Vec<ChannelId>> {
    host::roster_channels(node, space)
}

/// 받아 둔 체크포인트 스냅숏 블롭(최근 것부터).
pub(crate) fn snapshots(node: &Node, space: &SpaceId) -> Vec<BlobId> {
    host::snapshots_of(node, space)
        .into_iter()
        .map(|(_, _, b)| b)
        .collect()
}

/// 받아 둔 공간의 사용량(바이트).
pub(crate) fn used_bytes(node: &Node, space: &SpaceId) -> Option<u64> {
    host::used_bytes(node, space)
}

/// 멤버 쪽에서 보낼 삭제 반영·해제 수.
pub(crate) fn pending_counts(node: &Node, space: &SpaceId) -> (usize, usize) {
    member::pending_counts(node, space)
}
