//! 보관 전용 노드(X3) 시험 도우미: 받아 둔 공간·명단·진행 상태 들여다보기. 내용·키는 내보내지 않습니다.

use std::sync::Arc;

use yangpa_core::event::{SignedEvent, StreamId};
use yangpa_core::ids::{BlobId, ChannelId, EventId, SpaceId, UserId};
use yangpa_core::wire::LamportRange;

use crate::node::Node;

/// 받아 둔 공간인지(보관 노드 쪽).
pub fn is_hosted(node: &Node, space: &SpaceId) -> bool {
    crate::archive::is_hosted(node, space)
}

/// 보관 쪽 당겨오기·첨부 받기가 진행 중이 아닌지.
pub fn host_idle(node: &Node) -> bool {
    crate::archive::host_idle(node)
}

/// 받아 둔 공간 명단의 멤버(보관 노드 쪽).
pub fn roster_members(node: &Node, space: &SpaceId) -> Option<Vec<UserId>> {
    crate::archive::roster_members(node, space)
}

/// 받아 둔 명단에서 이 멤버가 들어온 논리 시계. 멤버가 아니면 `None`, 처음 멤버면 `Some(None)`.
pub fn roster_since(node: &Node, space: &SpaceId, user: &UserId) -> Option<Option<u64>> {
    crate::archive::roster_since(node, space, user)
}

/// 받아 둔 명단의 보관 채널(보관 노드 쪽).
pub fn roster_channels(node: &Node, space: &SpaceId) -> Option<Vec<ChannelId>> {
    crate::archive::roster_channels(node, space)
}

/// 받아 둔 체크포인트 스냅숏 블롭(보관 노드 쪽, 최근 것부터).
pub fn snapshots(node: &Node, space: &SpaceId) -> Vec<BlobId> {
    crate::archive::snapshots(node, space)
}

/// 받아 둔 공간의 사용량(바이트, 보관 노드 쪽).
pub fn used_bytes(node: &Node, space: &SpaceId) -> Option<u64> {
    crate::archive::used_bytes(node, space)
}

/// 이 이벤트를 시더 제공에서 뺐는지(보관 노드 쪽: 용량 때문에 본문을 빼고 받은 메시지).
pub fn seed_excluded(node: &Node, id: &EventId) -> bool {
    node.store.is_seed_excluded(id).unwrap_or(false)
}

/// 멤버 쪽에서 아직 보내지 못한 (삭제 반영, 해제) 수.
pub fn pending_counts(node: &Node, space: &SpaceId) -> (usize, usize) {
    crate::archive::pending_counts(node, space)
}

/// 저장소에 있는 이 공간의 이벤트 수(헤더만 남은 것 포함).
pub fn event_count(node: &Node, space: &SpaceId) -> u64 {
    node.store.event_count(space).unwrap_or(0)
}

/// 이 공간의 저장된 이벤트(본문 여부 그대로) 중 이 ID가 있는지와 본문이 있는지.
pub fn stored(node: &Node, id: &EventId) -> Option<bool> {
    node.store.event_meta(id).ok().flatten().map(|m| m.has_body)
}

/// 이 스트림의 저장된 이벤트(보관 노드에서 서명·헤더만 볼 때).
pub fn stream(node: &Node, space: &SpaceId, stream: StreamId) -> Vec<SignedEvent> {
    let range = LamportRange {
        start: 0,
        end: u64::MAX,
    };
    node.store
        .stream_events(space, stream, range, 4096)
        .unwrap_or_default()
}

/// 블롭 조각을 다 가졌는지.
pub fn blob_complete(node: &Node, blob: &BlobId) -> bool {
    node.store
        .blob_status(blob)
        .ok()
        .flatten()
        .is_some_and(|s| s.complete)
}

/// 이 공간에서 지금 인증된 연결이 있는 지정된 보관 노드 수(멤버 쪽).
pub fn online_archives(node: &Node, space: &SpaceId) -> usize {
    node.space_state(space)
        .map(|st| crate::archive::archive_peers(node, &st).len())
        .unwrap_or(0)
}

/// 주기 작업을 곧 돌립니다(멤버·보관 양쪽).
pub fn wake(node: &Arc<Node>) {
    crate::archive::wake(node);
}
