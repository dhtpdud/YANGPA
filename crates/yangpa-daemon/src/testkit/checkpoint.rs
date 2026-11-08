//! 상태 체크포인트 시험 도우미.

use std::sync::atomic::Ordering;

use yangpa_core::blob::BlobManifest;
use yangpa_core::body::Secret32;
use yangpa_core::checkpoint::OwnershipCert;
use yangpa_core::device::DeviceKeys;
use yangpa_core::ids::{BlobId, DeviceId, SpaceId, UserId};
use yangpa_core::time::Timestamp;
use yangpa_core::wire::DeviceKeysItem;

use crate::node::Node;

/// 해소기 바탕 체크포인트: (순번, 상태 해시, 경계). 없으면 `None`.
pub fn base(node: &Node, space: &SpaceId) -> Option<(u64, [u8; 32], u64)> {
    let core = node.spaces.read().get(space)?.core.clone();
    let c = core.lock();
    c.resolver.base().map(|b| (b.seq, b.state_hash, b.floor))
}

/// 저장한 바탕 기록: (순번, 스냅숏 블롭, 남겨 둔 스냅숏 블롭).
pub fn saved(node: &Node, space: &SpaceId) -> Option<(u64, BlobId, Vec<BlobId>)> {
    crate::checkpoint::saved_base(node, space).map(|b| (b.pin.seq, b.snapshot, b.kept))
}

/// 체크포인트를 내지 않게(`true`) 하거나 다시 내게 합니다.
pub fn hold(node: &Node, on: bool) {
    node.checkpoints.hold.store(on, Ordering::Relaxed);
}

/// 공간 스트림 경계.
pub fn floor(node: &Node, space: &SpaceId) -> u64 {
    crate::checkpoint::floor(node, space)
}

/// 체크포인트로 들어와 아직 다른 출처와 머리를 견주지 못했는지(그동안 이 공간에 쓰지 않음).
pub fn unconfirmed(node: &Node, space: &SpaceId) -> bool {
    crate::checkpoint::is_unconfirmed(node, space)
}

/// 늦은 이벤트로 거부된 것처럼 내 상태 이벤트를 다시 서명해 내게 합니다(한 이벤트는 재시작해도 한 번만).
pub fn resign(node: &std::sync::Arc<Node>, space: SpaceId, ids: Vec<yangpa_core::ids::EventId>) {
    crate::checkpoint::resign(node, space, ids);
}

/// 바탕 체크포인트의 키 머리 순번: (크루, 채널 `channel` 그룹). 없으면 `None`.
pub fn key_head_seqs(
    node: &Node,
    space: &SpaceId,
    channel: yangpa_core::ids::ChannelId,
) -> Option<(Option<u64>, Option<u64>)> {
    let state = node.space_state(space)?;
    let group = crate::keygroup::channel_group(node, space, &state, &channel);
    let core = node.spaces.read().get(space)?.core.clone();
    let c = core.lock();
    let b = c.resolver.base()?;
    let crew = yangpa_core::ids::KeyGroupId::crew(space);
    let seq_of = |g: &yangpa_core::ids::KeyGroupId| {
        b.key_heads.iter().find(|k| k.group == *g).map(|k| k.seq)
    };
    Some((seq_of(&crew), group.as_ref().and_then(seq_of)))
}

/// 채널 `channel` 그룹 사슬의 접은 바닥: (순번, 바닥 멤버를 아는지).
pub fn group_chain_base(
    node: &Node,
    space: &SpaceId,
    channel: yangpa_core::ids::ChannelId,
) -> Option<(u64, bool)> {
    let state = node.space_state(space)?;
    let group = crate::keygroup::channel_group(node, space, &state, &channel)?;
    let core = node.spaces.read().get(space)?.core.clone();
    let mut c = core.lock();
    c.keys.ensure_loaded(node, space);
    let b = c.keys.chains.get(&group)?.saved_base();
    Some((b.seq, b.members_known))
}

/// 가입 중 받아 둔 체크포인트가 있는지.
pub fn stashed(node: &Node, space: &SpaceId) -> bool {
    crate::checkpoint::has_stash(node, space)
}

/// 해소기 처리 기록 길이(바탕 뒤 항목 수): 체크포인트가 메모리를 묶는지 볼 때.
pub fn since_base(node: &Node, space: &SpaceId) -> Option<usize> {
    let core = node.spaces.read().get(space)?.core.clone();
    let c = core.lock();
    Some(c.resolver.since_base())
}

/// `GetDeviceKeys` 답을 가입한 쪽 규칙(키 약속·기기 트리 증명)으로 거릅니다.
pub fn accept_device_keys(
    node: &Node,
    space: &SpaceId,
    asked: &[DeviceId],
    items: Vec<DeviceKeysItem>,
) -> Vec<DeviceKeys> {
    crate::checkpoint::accept_reply(node, space, asked, items)
}

/// 이 노드의 신원·기기 키로 서명한 소유권 이전 인증서(소유자가 아니면 위조 시험용).
pub fn ownership_cert(node: &Node, space: SpaceId, to: UserId) -> Option<OwnershipCert> {
    let me = node.me().ok()?;
    Some(OwnershipCert::create(
        space,
        crate::checkpoint::base_id(node, &space),
        to,
        Timestamp::now(),
        &me.identity.signing,
        &me.device.signing,
    ))
}

/// 스냅숏 평문을 데몬과 같은 방법(상태 키에서 파생한 키, 결정론적 암호화)으로 블롭 암호문과 목록으로 만듭니다.
/// 규모 시뮬레이션이 새 멤버가 받는 블롭 크기를 잴 때 씁니다.
pub fn seal_snapshot(
    state_key: &Secret32,
    seq: u64,
    state_hash: &[u8; 32],
    plain: &[u8],
) -> crate::Result<(Vec<u8>, BlobManifest)> {
    crate::checkpoint::seal_snapshot(state_key, seq, state_hash, plain)
}

/// [`seal_snapshot`]으로 만든 블롭을 엽니다.
pub fn open_snapshot(
    state_key: &Secret32,
    seq: u64,
    state_hash: &[u8; 32],
    manifest: &BlobManifest,
    ct: &[u8],
) -> crate::Result<Vec<u8>> {
    crate::checkpoint::open_snapshot(state_key, seq, state_hash, manifest, ct)
}
