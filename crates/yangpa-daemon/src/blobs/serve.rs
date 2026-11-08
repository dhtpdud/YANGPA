//! 남의 블롭 요청에 답하기(닫힌 스웜의 시더).
//!
//! 순서: 인증된 기기인가 → 그 공간의 멤버·활성 기기인가 → 그 공간에서 내줄 수 있는 블롭인가
//! ([`super::serve_uses`]: 이 공간에 등록된 블롭의 키가 확인된 첨부 메시지·이모지·아이콘, 프로필 이미지로 등록된
//! 블롭을 쓰는 멤버 프로필; 아니면 있는지도 알리지 않고 `NotFound`) → 쓰임마다 시더 검사.
//! - 첨부: `check_seed_request`(채널을 볼 권한, 입장 이전 기록 공개, 시더 범위·개별 메시지 제외, 동시 전송·대역폭).
//!   지워진 메시지·휘발성 채널의 첨부는 내주지 않습니다.
//! - 이모지: 채널 없는 메시지 레인 요청으로 같은 검사.
//! - 아이콘·프로필 이미지: 시더가 켜져 있고 이미지 종류를 제공하는지, 동시 전송·대역폭만.
//! - 상태 체크포인트 스냅숏(최근 두 개): 멤버·보관 노드에게 시더 설정과 상관없이, 동시 전송·대역폭만.
//!
//! - 키 조각 블롭(`keygroup::shards`): 멤버 기기면 누구에게나(봉인돼 있어 제 몫만 엶).
//!
//! 매니페스트·비트맵 요청도 같은 검사를 거칩니다(제공하지 않는 블롭을 가졌는지 알리지 않게).

use std::sync::Arc;

use yangpa_core::blob::BlobManifest;
use yangpa_core::event::{Lane, StreamId};
use yangpa_core::ids::{BlobId, ChannelId, DeviceId, SpaceId, UserId};
use yangpa_core::model::SpaceState;
use yangpa_core::wire::{WireErrorCode, YpRequest, YpResponse};
use yangpa_net::PeerId;
use yangpa_sync::policy::{BUSY_RETRY_MS, DataType, SeedRequest, TokenBucket};

use super::{Use, fetch, now_ms};
use crate::node::Node;
use crate::{policy, sync_service};

#[derive(Clone, Copy)]
enum Ask {
    Manifest,
    Have,
    Chunk(u32),
}

pub(super) fn handle(node: &Arc<Node>, peer: PeerId, req: YpRequest) -> YpResponse {
    let (space, blob, ask) = match req {
        YpRequest::BlobManifest { space, blob } => (space, blob, Ask::Manifest),
        YpRequest::BlobHave { space, blob } => (space, blob, Ask::Have),
        YpRequest::BlobChunk { space, blob, index } => (space, blob, Ask::Chunk(index)),
        _ => return YpResponse::error(WireErrorCode::BadRequest, "블롭 요청이 아님"),
    };
    let Some((user, device)) = node.peers.identity_of(&peer) else {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증 전");
    };
    let Some(state) = node.space_state(&space) else {
        return YpResponse::error(WireErrorCode::NotFound, "모르는 공간");
    };
    // 지정된 보관 노드도 받습니다(첨부만, 보관 범위의 채널만: 시더 검사 고리).
    if (!state.is_member(&user) || !state.device_belongs(&user, &device))
        && !crate::archive::is_active(&state, &user, &device)
    {
        return YpResponse::error(WireErrorCode::NotMember, "멤버가 아님");
    }
    let manifest = match node.store.blob_manifest(&blob) {
        Ok(Some(m)) => m,
        Ok(None) => return YpResponse::error(WireErrorCode::NotFound, "없는 블롭"),
        Err(_) => return YpResponse::error(WireErrorCode::Internal, "저장소 오류"),
    };
    // 키 조각 블롭(크루 전체 재키의 기기 몫, `keygroup::shards`): 묶음마다 받는 기기에게 봉인돼 있어 멤버 기기면 누구에게나
    // 내줍니다(각자 제 조각만 엶). 확정 기기 한 곳에 몰리지 않게 받은 멤버도 내줍니다.
    if state.is_member(&user)
        && state.device_belongs(&user, &device)
        && crate::keygroup::shards::is_shard(node, &space, &blob)
    {
        // 요청 기기마다 바이트 예산(한 멤버가 블롭을 되풀이해 받아 재키 동안 업링크를 채우지 못하게).
        let cost = match ask {
            Ask::Manifest => 64 + 32 * manifest.chunks.len() as u64,
            Ask::Have => 64 + (manifest.chunks.len() as u64).div_ceil(8),
            Ask::Chunk(i) => manifest.chunk_len(i),
        };
        if !crate::keygroup::shards::allow_serve(node, device, cost) {
            return YpResponse::Busy {
                retry_after_ms: 5_000,
            };
        }
        return answer(node, &blob, &manifest, ask);
    }
    let chunks = manifest.chunks.len() as u64;
    let bytes = match ask {
        Ask::Manifest => 64 + 32 * chunks,
        Ask::Have => 64 + chunks.div_ceil(8),
        Ask::Chunk(i) if (i as u64) < chunks => manifest.chunk_len(i),
        Ask::Chunk(_) => return YpResponse::error(WireErrorCode::BadRequest, "조각 번호 범위 밖"),
    };
    let serving = sync_service::serving(node, device);
    let in_flight = serving.others;
    let resp = match authorize(
        node, &peer, &space, &state, user, device, &blob, bytes, in_flight,
    ) {
        Ok(channel) => {
            let r = answer(node, &blob, &manifest, ask);
            // 받는 사람별 속도(앱이 첨부 옆에 보임).
            if let YpResponse::Chunk { data } = &r {
                crate::transfer::record_upload(
                    node,
                    &space,
                    channel.as_ref(),
                    &blob,
                    &user,
                    data.len() as u64,
                );
                crate::badges::add_seeded(node, &user, data.len() as u64);
            }
            r
        }
        Err(r) => r,
    };
    drop(serving);
    resp
}

/// 쓰임 중 하나라도 통과하면 제공합니다. 모두 거절이면 첫 거절 이유. 통과하면 그 쓰임의 채널(첨부일 때).
#[allow(clippy::too_many_arguments, clippy::result_large_err)]
fn authorize(
    node: &Node,
    peer: &PeerId,
    space: &SpaceId,
    state: &SpaceState,
    user: UserId,
    device: DeviceId,
    blob: &BlobId,
    bytes: u64,
    in_flight: u32,
) -> Result<Option<ChannelId>, YpResponse> {
    let uses = super::serve_uses(node, space, state, blob);
    if uses.is_empty() {
        return Err(YpResponse::error(WireErrorCode::NotFound, "없는 블롭"));
    }
    let now = now_ms();
    let mut first = None;
    for u in uses {
        // P2P 전용 채널의 첨부는 함께 참가한 P2P 피어에게만, 나머지는 Tor 피어에게만 줍니다.
        let stream = match &u {
            Use::Attachment(r) => StreamId::Channel(r.channel),
            _ => StreamId::Space,
        };
        if !crate::p2p::route_ok(node, peer, space, state, &stream) {
            first.get_or_insert_with(|| YpResponse::error(WireErrorCode::NotFound, "없는 블롭"));
            continue;
        }
        let channel = match &u {
            Use::Attachment(r) => Some(r.channel),
            _ => None,
        };
        let r = match u {
            Use::Attachment(r) => {
                if state
                    .channels
                    .get(&r.channel)
                    .is_none_or(|c| c.is_volatile())
                {
                    Err(YpResponse::error(WireErrorCode::NotFound, "없는 채널"))
                } else if node.store.is_deleted(&r.event).unwrap_or(true) {
                    Err(YpResponse::error(WireErrorCode::NotFound, "지워진 메시지"))
                } else {
                    let req = SeedRequest {
                        state,
                        requester: user,
                        requester_device: device,
                        lane: Lane::Message,
                        channel: Some(r.channel),
                        data_type: r.data_type,
                        event: Some(r.event),
                        event_lamport: Some(r.lamport),
                        requester_since: sync_service::requester_since(node, space, &user),
                        channel_opened: policy::channel_opened(node, space, state, &r.channel),
                        bytes,
                        in_flight,
                        now_ms: now,
                    };
                    sync_service::check(node, space, &req)
                }
            }
            Use::Emoji => {
                let req = SeedRequest {
                    state,
                    requester: user,
                    requester_device: device,
                    lane: Lane::Message,
                    channel: None,
                    data_type: DataType::Emoji,
                    event: None,
                    event_lamport: None,
                    requester_since: None,
                    channel_opened: true,
                    bytes,
                    in_flight,
                    now_ms: now,
                };
                sync_service::check(node, space, &req)
            }
            Use::Icon | Use::Profile if crate::archive::is_active(state, &user, &device) => Err(
                YpResponse::error(WireErrorCode::Forbidden, "보관 노드에는 주지 않는 이미지"),
            ),
            Use::Icon | Use::Profile => check_image(node, space, &user, bytes, in_flight, now),
            // 상태 스냅숏은 시더 설정과 상관없이 멤버·보관 노드에게 줍니다(체크포인트로 들어오는 길). 대역폭만 셉니다.
            Use::Snapshot => check_bandwidth(node, space, &user, bytes, in_flight, now),
        };
        match r {
            Ok(()) => return Ok(channel),
            Err(e) => {
                first.get_or_insert(e);
            }
        }
    }
    Err(first.unwrap_or_else(|| YpResponse::error(WireErrorCode::NotFound, "없는 블롭")))
}

/// 채널에 묶이지 않은 이미지(공간 아이콘, 멤버 프로필 이미지): 시더 켜짐·이미지 종류·동시 전송·대역폭.
#[allow(clippy::result_large_err)]
fn check_image(
    node: &Node,
    space: &SpaceId,
    user: &UserId,
    bytes: u64,
    in_flight: u32,
    now: u64,
) -> Result<(), YpResponse> {
    let p = policy::seeder_policy(node, space);
    if !p.enabled || !p.types.contains(&DataType::Image) {
        return Err(YpResponse::error(
            WireErrorCode::NotSeeded,
            "이 시더가 제공하지 않는 데이터",
        ));
    }
    check_bandwidth(node, space, user, bytes, in_flight, now)
}

/// 동시 전송·대역폭 검사(시더 켜짐·종류는 보지 않음).
#[allow(clippy::result_large_err)]
fn check_bandwidth(
    node: &Node,
    space: &SpaceId,
    user: &UserId,
    bytes: u64,
    in_flight: u32,
    now: u64,
) -> Result<(), YpResponse> {
    let p = policy::seeder_policy(node, space);
    if in_flight >= p.max_concurrent.max(1) {
        return Err(YpResponse::Busy {
            retry_after_ms: BUSY_RETRY_MS,
        });
    }
    use crate::ratelimit::{self, Dir};
    let scoped = ratelimit::wait_ms(node, Dir::Up, space, None, Some(user), bytes, now);
    if scoped > 0 {
        return Err(YpResponse::Busy {
            retry_after_ms: scoped,
        });
    }
    {
        let mut bucket = node.sync.bucket.lock();
        let b = bucket.get_or_insert_with(|| TokenBucket::new(p.upload_limit_bps, now));
        b.configure(p.upload_limit_bps, now);
        b.try_consume(bytes, now)
            .map_err(|retry_after_ms| YpResponse::Busy { retry_after_ms })?;
    }
    ratelimit::consume(node, Dir::Up, space, None, Some(user), bytes, now);
    Ok(())
}

fn answer(node: &Node, blob: &BlobId, manifest: &BlobManifest, ask: Ask) -> YpResponse {
    if let Ask::Chunk(_) = ask {
        // 내주는 중인 블롭은 잠시 저장 공간 정리에서 뺍니다.
        super::cache::touch(node, blob);
    }
    match ask {
        Ask::Manifest => YpResponse::Manifest(manifest.clone()),
        Ask::Have => match node.store.blob_have(blob) {
            Ok(Some((bm, total))) => YpResponse::Have {
                bitmap: fetch::from_store_bitmap(&bm, total).to_wire().0,
                total,
            },
            Ok(None) => YpResponse::error(WireErrorCode::NotFound, "없는 블롭"),
            Err(_) => YpResponse::error(WireErrorCode::Internal, "저장소 오류"),
        },
        Ask::Chunk(i) => match node.store.blob_read_chunk(blob, i) {
            Ok(Some(mut data)) => {
                if super::corrupt_uploads(node)
                    && let Some(b) = data.first_mut()
                {
                    *b ^= 0x01;
                }
                YpResponse::Chunk { data }
            }
            Ok(None) => YpResponse::error(WireErrorCode::NotFound, "없는 조각"),
            Err(_) => YpResponse::error(WireErrorCode::Internal, "저장소 오류"),
        },
    }
}
