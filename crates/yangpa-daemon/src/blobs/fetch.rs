//! 블롭 하나 내려받기(닫힌 스웜): 매니페스트 → 공간 멤버 기기들의 보유 비트맵 → 희귀 조각 우선으로 여러 피어에게
//! 나눠 요청 → 조각마다 매니페스트 해시로 검증해 저장합니다. 검증에 실패한 조각은 보낸 피어를 감점하고 다른
//! 피어에게 다시 받습니다. 저장소의 조각 비트맵에서 시작하므로 재시작하면 받은 데까지 이어 받습니다.
//!
//! 비트맵 순서: 저장소는 바이트마다 높은 비트부터(`0x80 >> (i % 8)`), 네트워크(`YpResponse::Have`)와
//! `yangpa_sync::swarm::Bitmap`은 낮은 비트부터(`1 << (i % 8)`) 입니다. 여기와 `serve`에서 바꿉니다.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use tracing::debug;
use yangpa_core::blob::BlobManifest;
use yangpa_core::ids::BlobId;
use yangpa_core::model::SecurityLevel;
use yangpa_core::wire::{WireErrorCode, YpRequest, YpResponse};
use yangpa_net::{NetError, PeerId};
use yangpa_rpc::api::Notification;
use yangpa_store::PersistentBlob;
use yangpa_sync::score::Penalty;
use yangpa_sync::swarm::{Bitmap, ChunkOutcome, ChunkScheduler, RequestError, SwarmConfig};

use super::{PROFILE_SPACE, Want, now_ms};
use crate::filelink::{self, Route};
use crate::node::Node;

/// 비트맵을 자주 다시 물을 때(가진 피어가 없는 조각이 있거나 쓸 피어가 없음)와 평소 간격.
const REQUERY_FAST_MS: u64 = 2_000;
const REQUERY_SLOW_MS: u64 = 10_000;
/// 요청 중인 것 없이 이만큼 진전이 없으면 그만두고 나중에 다시 시도합니다.
const STALL_MS: u64 = 15_000;
/// 자동 내려받기에서 메시지에 적힌 크기보다 이만큼 넘게 큰 블롭은 받지 않습니다(크기를 속인 첨부).
const SIZE_SLACK: u64 = 64 * 1024;

pub(super) enum Outcome {
    Complete,
    /// 나중에 다시 시도(받을 피어 없음 등).
    Retry,
    /// 받을 수 없거나 받으면 안 되는 블롭.
    Abandon,
    /// 더 받을 필요가 없어짐(받을 블롭에서 빠짐, 종료).
    Cancelled,
    /// 저장 공간 한도 안에 자리가 없음(자동 내려받기만, `cache::admit_download`).
    NoRoom,
}

/// 저장소 비트맵(높은 비트부터)에서 조각 `i`를 가졌는지.
pub(super) fn store_bit(bytes: &[u8], i: u32) -> bool {
    bytes
        .get((i / 8) as usize)
        .is_some_and(|b| b & (0x80 >> (i % 8)) != 0)
}

/// 저장소 비트맵(높은 비트부터) → 스웜 비트맵(낮은 비트부터).
pub(super) fn from_store_bitmap(bytes: &[u8], total: u32) -> Bitmap {
    let mut bm = Bitmap::new(total);
    for i in (0..total).filter(|i| store_bit(bytes, *i)) {
        bm.set(i);
    }
    bm
}

enum Reply {
    Have(PeerId, Result<YpResponse, NetError>),
    Chunk(Route, u32, Result<YpResponse, NetError>),
}

fn ask_have(node: &Arc<Node>, peer: PeerId, req: YpRequest) -> BoxFuture<'static, Reply> {
    let node = node.clone();
    Box::pin(async move { Reply::Have(peer, crate::p2p::request(&node, peer, req).await) })
}

/// `ticket`: 주 연결로 보내는 조각의 피어당 자리(`chunk_ticket`). 파일 회로는 스웜이 따로라 자리를 쓰지 않습니다.
fn ask_chunk(
    node: &Arc<Node>,
    route: Route,
    index: u32,
    req: YpRequest,
    ticket: Option<super::ChunkTicket>,
) -> BoxFuture<'static, Reply> {
    let node = node.clone();
    Box::pin(async move {
        let r = filelink::send(&node, route, req).await;
        drop(ticket);
        Reply::Chunk(route, index, r)
    })
}

/// 멤버 기기들에게 매니페스트를 묻고 처음 온 올바른 것을 씁니다.
async fn fetch_manifest(
    node: &Arc<Node>,
    blob: &BlobId,
    w: &Want,
) -> Result<Arc<BlobManifest>, Outcome> {
    let start = now_ms();
    loop {
        if node.is_shutting_down() || !super::is_wanted(node, blob) {
            return Err(Outcome::Cancelled);
        }
        let mut asks: FuturesUnordered<_> = super::member_peers(node, &w.space, w.channel.as_ref())
            .into_iter()
            .map(|peer| {
                let node = node.clone();
                let req = YpRequest::BlobManifest {
                    space: w.space,
                    blob: *blob,
                };
                async move { (peer, crate::p2p::request(&node, peer, req).await) }
            })
            .collect();
        while let Some((peer, r)) = asks.next().await {
            if let Ok(YpResponse::Manifest(m)) = r {
                if m.id() == *blob && m.check().is_ok() && !m.chunks.is_empty() {
                    return Ok(Arc::new(m));
                }
                // 요청한 ID와 맞지 않는 매니페스트.
                super::penalize(node, peer, Penalty::BadData);
            }
        }
        if now_ms().saturating_sub(start) >= STALL_MS {
            return Err(Outcome::Retry);
        }
        tokio::time::sleep(Duration::from_millis(REQUERY_FAST_MS)).await;
    }
}

/// 받은 조각 + 요청 중인 조각이 시험용 상한에 닿았는지.
fn held(node: &Node, sched: &ChunkScheduler<Route>) -> bool {
    super::hold_at(node).is_some_and(|n| sched.progress().0 + sched.in_flight() >= n)
}

pub(super) async fn download(node: &Arc<Node>, blob: BlobId, w: Want) -> Outcome {
    let Some(state) = node.space_state(&w.space) else {
        return Outcome::Abandon;
    };
    let security = match &w.channel {
        Some(c) => match state.channels.get(c) {
            Some(ch) if !ch.is_volatile() => ch.security,
            // 휘발성 채널의 첨부는 디스크에 두지 않습니다(그 경로는 B2의 휘발성 저장소).
            _ => return Outcome::Abandon,
        },
        None => SecurityLevel::Normal,
    };
    drop(state);

    // 이미 가진 블롭(다른 공간·용도로 등록됨)이면 이 공간에 새로 등록하지 않습니다. 등록은 "이 공간에서 내줘도
    // 되는 블롭"이라는 뜻이라, 다른 멤버가 남의 블롭 ID를 이 공간 메시지에 적었다고 그 블롭이 이 공간에 내줘지면
    // 안 됩니다. 처음 받는 블롭은 이 공간 멤버에게서 매니페스트를 받았으니 이 공간에 등록합니다.
    let (manifest, fresh) = match node.store.blob_manifest(&blob) {
        Ok(Some(m)) => (m, false),
        Ok(None) => match fetch_manifest(node, &blob, &w).await {
            Ok(m) => (m, true),
            Err(o) => return o,
        },
        Err(e) => {
            debug!(blob = %blob.short(), error = %e, "매니페스트 읽기 실패");
            return Outcome::Retry;
        }
    };
    if !w.manual
        && yangpa_crypto::blob::plaintext_len(&manifest) > w.size.saturating_add(SIZE_SLACK)
    {
        debug!(blob = %blob.short(), "적힌 크기보다 큰 블롭: 자동으로 받지 않음");
        return Outcome::Abandon;
    }
    let pb = match PersistentBlob::new((*manifest).clone(), security) {
        Ok(pb) => pb,
        Err(_) => return Outcome::Abandon,
    };
    if fresh {
        let reg_space = if w.profile { PROFILE_SPACE } else { w.space };
        if let Err(e) = node.store.blob_register(&reg_space, &pb) {
            debug!(blob = %blob.short(), error = %e, "블롭 등록 실패");
            return Outcome::Retry;
        }
        if w.data_type == yangpa_sync::policy::DataType::Emoji {
            let _ = node.store.blob_set_pinned(&blob, true);
        }
    }
    // 정리 잠금을 기다리거나 파일을 지울 수 있어 블로킹 스레드에서.
    let admitted = {
        let (m, want) = (manifest.clone(), w.clone());
        super::cache::run_blocking(node, move |n| {
            super::cache::admit_download(n, &blob, &m, &want)
        })
        .await
        .unwrap_or(false)
    };
    if !admitted && !w.manual {
        debug!(blob = %blob.short(), "저장 공간 한도: 자동으로 받지 않음");
        return Outcome::NoRoom;
    }

    let total = manifest.chunks.len() as u32;
    let local = match node.store.blob_have(&blob) {
        Ok(Some((bm, t))) if t == total => from_store_bitmap(&bm, total),
        _ => return Outcome::Retry,
    };
    if local.is_full() {
        return Outcome::Complete;
    }
    let mut cfg = SwarmConfig {
        // 네트워크 요청 시간 제한보다 조금 길게(그 안에 오는 답을 시간 초과로 버리지 않게).
        request_timeout_ms: node.cfg.net.request_timeout.as_millis() as u64 + 5_000,
        seed: rand::random(),
        ..SwarmConfig::default()
    };
    // Tor로 돌면 시간 제한은 Tor 스웜에 맞춥니다(직접 연결로 못 보낸 요청은 Tor로 감). 동시 요청 수는 받을 피어의
    // 연결 종류로 아래에서 정합니다(`chunk_window`: Tor 회로 하나를 메시지·동기화와 같이 쓰면 작게, 직접 연결이면
    // 잰 속도에 맞춤).
    if let Some(t) = crate::tor::tor_download_limits(node) {
        cfg.request_timeout_ms = cfg
            .request_timeout_ms
            .max(t.request_timeout.as_millis() as u64);
    }
    // 큰 블롭을 Tor로 받으면 조각은 글과 다른 파일 회로로(`filelink`). 회로는 조각을 가졌다고 답한 상대에게만 엽니다
    // ([`filelink::targets`]). 이어지기 전과 끊긴 뒤, 직접 연결로 받을 때는 주 연결로.
    let file_link = filelink::wanted(node, manifest.chunks.len());
    let Ok(mut sched) = ChunkScheduler::<Route>::with_local(local, cfg) else {
        return Outcome::Abandon;
    };
    let mut routes = filelink::Routes::new(total);

    let mut pending: FuturesUnordered<BoxFuture<'static, Reply>> = FuturesUnordered::new();
    let mut asking: HashSet<PeerId> = HashSet::new();
    let mut seeders: HashSet<PeerId> = HashSet::new();
    let mut next_query = 0u64;
    let mut last_progress = now_ms();
    let mut throttled_until = 0u64;
    let mut members: Vec<PeerId> = Vec::new();
    let mut rewindow = true;
    let mut pacer = super::Pacer::new(manifest.chunk_size);
    loop {
        if node.is_shutting_down() || !super::is_wanted(node, &blob) {
            return Outcome::Cancelled;
        }
        let now = now_ms();
        // 기한이 지난 요청은 스케줄러가 실패로 셉니다(늦게 온 답은 아래에서 따로 받음).
        sched.check_timeouts(now);
        if now >= next_query {
            members = super::member_peers(node, &w.space, w.channel.as_ref());
            rewindow = true;
            if file_link && !pacer.is_direct() {
                filelink::prepare(node, &filelink::targets(node, &seeders));
            }
            for peer in members.iter().copied() {
                if asking.insert(peer) {
                    let req = YpRequest::BlobHave {
                        space: w.space,
                        blob,
                    };
                    pending.push(ask_have(node, peer, req));
                }
            }
            next_query = now
                + if sched.unavailable_chunks() > 0 || sched.usable_peers() == 0 {
                    REQUERY_FAST_MS
                } else {
                    REQUERY_SLOW_MS
                };
        }
        if std::mem::take(&mut rewindow) {
            // 가진 피어가 알려지면 그들로, 아직이면 물어본 멤버로 정합니다.
            let peers: Vec<PeerId> = if seeders.is_empty() {
                members.clone()
            } else {
                seeders.iter().copied().collect()
            };
            pacer.update(node, &peers);
        }
        routes.steer(node, &mut pacer, &mut sched, file_link, now);
        let holding = held(node, &sched);
        if !holding && now >= throttled_until {
            let asked = sched.progress().0 + sched.in_flight();
            let mut reqs = sched.next_requests(now);
            // 시험용 멈춤: 정확히 그 수까지만 요청하고 나머지는 바쁜 것으로 돌립니다.
            if let Some(n) = super::hold_at(node) {
                let room = n.saturating_sub(asked) as usize;
                for (route, i) in reqs.split_off(room.min(reqs.len())) {
                    let busy = RequestError::Busy {
                        retry_after_ms: 1_000,
                    };
                    sched.on_error(&route, i, busy, now);
                }
            }
            // 주 연결로 이 피어에게 보내 둔 조각이 노드 전체 상한이면(다른 블롭이 쓰는 중) 감점 없이 잠깐 쉽니다.
            let reqs: Vec<_> = reqs
                .into_iter()
                .filter_map(|(route, i)| {
                    if route.1 != filelink::MAIN {
                        return Some((route, i, None));
                    }
                    match super::chunk_ticket(node, route.0) {
                        Some(t) => Some((route, i, Some(t))),
                        None => {
                            let busy = RequestError::Busy {
                                retry_after_ms: super::LOCAL_BUSY_MS,
                            };
                            sched.on_error(&route, i, busy, now);
                            None
                        }
                    }
                })
                .collect();
            if !reqs.is_empty() {
                let (send, over) =
                    super::admit_chunks(node, &w.space, w.channel.as_ref(), now, reqs, |i| {
                        manifest.chunk_len(i)
                    });
                // 내 수신 상한을 넘는 요청은 보내지 않고 대기열로 돌립니다.
                for (route, i, ms) in over {
                    sched.on_error(&route, i, RequestError::Busy { retry_after_ms: ms }, now);
                    throttled_until = throttled_until.max(now + u64::from(ms));
                }
                for (route, i, t) in send {
                    let req = YpRequest::BlobChunk {
                        space: w.space,
                        blob,
                        index: i,
                    };
                    pending.push(ask_chunk(node, route, i, req, t));
                }
            }
        }
        let idle = !holding && sched.in_flight() == 0;
        if idle && now.saturating_sub(last_progress) >= STALL_MS {
            return Outcome::Retry;
        }
        let mut wake_at = next_query;
        if idle {
            wake_at = wake_at.min(last_progress + STALL_MS);
        }
        if let Some(d) = sched.next_deadline() {
            wake_at = wake_at.min(d);
        }
        // 바쁘다고 답한 피어가 쉬는 동안·마지막 단계에서 보낼 요청이 없어도, 보낼 수 있게 되면 바로 이어 요청합니다.
        if let Some(d) = sched.next_wake(now) {
            wake_at = wake_at.min(d);
        }
        if throttled_until > now {
            wake_at = wake_at.min(throttled_until);
        }
        if holding {
            // 시험용 멈춤: 풀리면 곧 이어 가도록 짧게 다시 봅니다.
            wake_at = wake_at.min(now + 100);
        }
        let sleep = tokio::time::sleep(Duration::from_millis(
            wake_at.saturating_sub(now).clamp(5, REQUERY_SLOW_MS),
        ));
        let reply = tokio::select! {
            r = pending.next(), if !pending.is_empty() => r,
            _ = sleep => None,
        };
        let Some(reply) = reply else { continue };
        let now = now_ms();
        match reply {
            Reply::Have(peer, r) => {
                asking.remove(&peer);
                match r {
                    Ok(YpResponse::Have { bitmap, total: t }) if t == total => {
                        if let Ok(bm) = Bitmap::from_wire(&bitmap, total) {
                            rewindow |= if bm.count_ones() > 0 {
                                seeders.insert(peer)
                            } else {
                                seeders.remove(&peer)
                            };
                            routes.set_have(&mut sched, peer, bm);
                        }
                    }
                    // 제공하지 않는 피어: 가진 것이 없는 것으로 둡니다.
                    Ok(YpResponse::Error { .. }) => {
                        rewindow |= seeders.remove(&peer);
                        routes.set_have(&mut sched, peer, Bitmap::new(total));
                    }
                    _ => {}
                }
                super::record_seeders(node, &blob, seeders.len());
                if file_link && !pacer.is_direct() {
                    filelink::prepare(node, &filelink::targets(node, &seeders));
                }
            }
            Reply::Chunk(route, i, r) => match r {
                Ok(YpResponse::Chunk { data }) => {
                    let peer = route.0;
                    pacer.record(now, data.len());
                    let verified = manifest.verify_chunk(i, &data);
                    match sched.on_chunk(&route, i, verified) {
                        ChunkOutcome::Accepted { .. } => {
                            if let Err(e) = node.store.blob_write_chunk(&blob, i, &data) {
                                debug!(blob = %blob.short(), error = %e, "조각 저장 실패");
                                return Outcome::Retry;
                            }
                            last_progress = now;
                            super::record_chunk(node, &blob, peer);
                            super::reward(node, peer);
                            let (have, total) = sched.progress();
                            node.notify.publish(Notification::BlobProgress {
                                space: w.space,
                                blob,
                                have,
                                total,
                            });
                        }
                        ChunkOutcome::BadChunk { excluded, .. } => {
                            super::record_bad(node, &blob);
                            super::penalize(node, peer, Penalty::BadData);
                            debug!(blob = %blob.short(), %peer, index = i, excluded, "검증 실패 조각: 다른 피어에게 다시 받음");
                        }
                        ChunkOutcome::Unexpected if verified && !sched.local_bitmap().get(i) => {
                            // 시간 초과로 버린 요청의 늦은 답: 검증됐으면 그대로 씁니다.
                            if node.store.blob_write_chunk(&blob, i, &data).is_ok() {
                                let _ = sched.mark_have(i);
                                last_progress = now;
                                super::record_chunk(node, &blob, peer);
                            }
                        }
                        ChunkOutcome::Duplicate | ChunkOutcome::Unexpected => {}
                    }
                }
                Ok(YpResponse::Busy { retry_after_ms }) => {
                    super::record_busy(node, &blob);
                    sched.on_error(&route, i, RequestError::Busy { retry_after_ms }, now);
                }
                // 이 노드의 망 계층이 그 피어에게 보내 둔 요청 상한을 채움(글·동기화와 나눠 씀): 상대 잘못이 아님.
                Err(NetError::Busy) => {
                    let busy = RequestError::Busy {
                        retry_after_ms: super::LOCAL_BUSY_MS,
                    };
                    sched.on_error(&route, i, busy, now);
                }
                Ok(YpResponse::Error {
                    code: WireErrorCode::NotFound,
                    ..
                }) => {
                    sched.on_error(&route, i, RequestError::NotFound, now);
                }
                Ok(YpResponse::Error { .. }) => {
                    sched.on_error(&route, i, RequestError::Denied, now);
                }
                _ => {
                    sched.on_error(&route, i, RequestError::Failed, now);
                }
            },
        }
        if sched.is_complete() {
            return Outcome::Complete;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_bitmap_order() {
        // 저장소: 조각 0 = 0x80, 조각 9 = 둘째 바이트의 0x40.
        let bm = from_store_bitmap(&[0x80, 0x40], 12);
        let ones: Vec<u32> = bm.iter_ones().collect();
        assert_eq!(ones, vec![0, 9]);
        // 네트워크(낮은 비트부터) 형식으로는 0x01, 0x02.
        assert_eq!(bm.to_wire().0, vec![0x01, 0x02]);
    }
}
