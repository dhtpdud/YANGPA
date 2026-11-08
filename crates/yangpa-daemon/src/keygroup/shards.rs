//! 크루 트리 전체 재키의 기기 몫(`DeviceBundle`)을 사용자별 조각 블롭 하나로 냅니다(protocol.md "봉인 묶음").
//!
//! 블롭은 64KiB 조각으로 나뉜 내용 주소 블롭(기존 블롭 저장·전송을 그대로 씀)이고, 공간 스트림에는 갱신 머리(블롭 ID를
//! 실은 `KeyUpdate`)만 갑니다.
//! - 머리 조각 `H`개: postcard (판, H, 자료 조각마다 첫 사용자 ID).
//! - 자료 조각: postcard `Vec<(사용자, Vec<묶음>)>`, 0으로 채움. 사용자 하나는 한 조각 안에만 둡니다.
//!
//! 기기는 먼저 자기 사용자 몫만 묻습니다(`GetKeyShard`, 몇 KB: 답하는 쪽이 머리와 그 조각을 읽어 그 사용자 묶음만
//! 돌려줌). 답이 없거나 비면(옛 노드, 블롭이 없는 멤버) 머리와 자기 사용자 조각을 받습니다(4.5만 대 크루에서 블롭 전체는
//! 100MB 남짓, 이 길로 기기마다 받는 것은 128KiB 남짓). 조각은 매니페스트 해시로, 묶음은 갱신의 `bundle_root` 증명으로
//! 확인하므로 누가 내주든 같습니다. 확정 기기는 블롭을 고정해 두고 같은 공간의 활성 멤버 기기에게 내주며, 받은 멤버도
//! 받은 조각을 내줍니다. 내주는 쪽은 요청 기기마다 바이트 예산을 둡니다([`allow_serve`]): 한 멤버가 블롭을 되풀이해
//! 받아 재키 동안 확정 기기 업링크를 채우지 못하게.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::debug;
use yangpa_core::blob::BlobManifest;
use yangpa_core::ids::{BlobId, DeviceId, KeyGroupId, SpaceId, UserId};
use yangpa_core::model::SecurityLevel;
use yangpa_core::wire::{YpRequest, YpResponse};
use yangpa_crypto::DeviceBundle;
use yangpa_net::PeerId;
use yangpa_store::PersistentBlob;

use crate::error::{Error, Result};
use crate::ingest;
use crate::node::Node;

/// 묶음이 이보다 많을 때만 조각 블롭으로 냅니다(적으면 받은편지함 묶음이 더 쌈).
pub(crate) const SHARD_MIN_BUNDLES: usize = 64;
/// 조각 크기.
const PAGE: u32 = 64 * 1024;
/// 공간마다 남겨 두는 조각 블롭 수(늦게 온 기기가 바로 앞 재키의 몫도 받게).
const KEEP_BLOBS: usize = 2;
const SHARD_VERSION: u8 = 1;
/// 한 번에 물어보는 피어 수.
const FETCH_PEERS: usize = 4;
/// 막혀 있을 때 묻는 바퀴 수와 그 사이.
const FETCH_ROUNDS: usize = 4;
const FETCH_AGAIN: std::time::Duration = std::time::Duration::from_secs(20);
/// 조각 블롭을 내주는 예산(요청 기기마다): 평소 속도와 한 번에 쓸 수 있는 양. 머리·매니페스트·조각 하나를 받는 정직한
/// 기기는 한 번에 끝납니다.
const SERVE_RATE_BPS: u64 = 32 * 1024;
const SERVE_BURST: u64 = 512 * 1024;
/// `GetKeyShard` 답 하나를 예산에서 셀 크기(머리·조각을 읽는 일).
const SHARD_ASK_COST: u64 = 32 * 1024;
/// 예산을 기억하는 요청 기기 수 상한(넘으면 비움).
const MAX_BUDGETS: usize = 8192;

#[derive(Serialize, Deserialize)]
struct Prefix {
    v: u8,
    header_pages: u32,
}

#[derive(Serialize, Deserialize)]
struct Header {
    v: u8,
    header_pages: u32,
    firsts: Vec<UserId>,
}

type Page = Vec<(UserId, Vec<Vec<u8>>)>;

fn setting(space: &SpaceId) -> String {
    format!("kg.shards.{space}")
}

fn listed(node: &Node, space: &SpaceId) -> Vec<BlobId> {
    node.store
        .get_setting(&setting(space))
        .ok()
        .flatten()
        .unwrap_or_default()
}

fn remember(node: &Node, space: &SpaceId, blob: BlobId) {
    let mut list = listed(node, space);
    if list.contains(&blob) {
        return;
    }
    list.push(blob);
    while list.len() > KEEP_BLOBS {
        let old = list.remove(0);
        let _ = node.store.blob_set_pinned(&old, false);
        let _ = node.store.blob_delete(&old);
    }
    let _ = node.store.put_setting(&setting(space), &list);
}

/// 이 공간의 마지막 조각 블롭(시험·성능 측정용).
pub(crate) fn last_blob(node: &Node, space: &SpaceId) -> Option<BlobId> {
    listed(node, space).last().copied()
}

/// 이 공간의 키 조각 블롭인지(블롭 시더가 멤버 기기에게 바로 내줌).
pub(crate) fn is_shard(node: &Node, space: &SpaceId, blob: &BlobId) -> bool {
    listed(node, space).contains(blob)
}

pub(crate) fn forget_space(node: &Node, space: &SpaceId) {
    for b in listed(node, space) {
        let _ = node.store.blob_set_pinned(&b, false);
        let _ = node.store.blob_delete(&b);
    }
    let _ = node.store.delete_setting(&setting(space));
}

/// 기기 몫을 사용자별 조각 블롭으로 만들어 저장·고정합니다. 묶음이 적거나 한 사용자 몫이 조각 하나를 넘으면 `None`
/// (부르는 쪽이 받은편지함으로 보냄).
pub(crate) fn publish(
    node: &Node,
    space: &SpaceId,
    bundles: &[DeviceBundle],
    owners: &HashMap<DeviceId, UserId>,
) -> Result<Option<BlobId>> {
    if bundles.len() <= SHARD_MIN_BUNDLES {
        return Ok(None);
    }
    let mut by_user: BTreeMap<UserId, Vec<Vec<u8>>> = BTreeMap::new();
    for b in bundles {
        if let Some(u) = owners.get(&b.device) {
            by_user.entry(*u).or_default().push(b.encode());
        }
    }
    let page = PAGE as usize;
    let mut pages: Vec<Vec<u8>> = Vec::new();
    let mut firsts = Vec::new();
    let mut cur: Page = Vec::new();
    let mut cur_len = 8usize;
    for (u, list) in by_user {
        let one = postcard::to_stdvec(&(u, &list))
            .map_err(|e| Error::internal(format!("조각 직렬화: {e}")))?;
        if one.len() + 8 > page {
            debug!(space = %space.short(), "한 사용자 몫이 조각 하나를 넘어 받은편지함으로 보냄");
            return Ok(None);
        }
        if cur_len + one.len() > page && !cur.is_empty() {
            pages.push(seal_page(&cur, page)?);
            cur.clear();
            cur_len = 8;
        }
        if cur.is_empty() {
            firsts.push(u);
        }
        cur_len += one.len();
        cur.push((u, list));
    }
    if !cur.is_empty() {
        pages.push(seal_page(&cur, page)?);
    }
    // 머리 조각 수는 머리 길이에 따라 정해지므로 늘려 가며 맞춥니다(대개 1).
    let mut header_pages = 1u32;
    let head = loop {
        let h = Header {
            v: SHARD_VERSION,
            header_pages,
            firsts: firsts.clone(),
        };
        let bytes =
            postcard::to_stdvec(&h).map_err(|e| Error::internal(format!("조각 머리: {e}")))?;
        let need = bytes.len().div_ceil(page).max(1) as u32;
        if need <= header_pages {
            break bytes;
        }
        header_pages = need;
    };
    let mut content = head;
    content.resize(header_pages as usize * page, 0);
    for p in &pages {
        content.extend_from_slice(p);
    }
    let manifest = BlobManifest::build(&content, PAGE);
    let id = manifest.id();
    let pb = PersistentBlob::new(manifest, SecurityLevel::Normal)?;
    node.store.blob_register(space, &pb)?;
    for (i, c) in content.chunks(page).enumerate() {
        node.store.blob_write_chunk(&id, i as u32, c)?;
    }
    node.store.blob_set_pinned(&id, true)?;
    remember(node, space, id);
    Ok(Some(id))
}

fn seal_page(p: &Page, page: usize) -> Result<Vec<u8>> {
    let mut v = postcard::to_stdvec(p).map_err(|e| Error::internal(format!("조각: {e}")))?;
    if v.len() > page {
        return Err(Error::internal("조각이 넘침"));
    }
    v.resize(page, 0);
    Ok(v)
}

/// 조각 블롭 하나의 크기(시험·성능 측정용, `testkit::ShardStats`로 내보냄).
#[derive(Clone, Debug, Default)]
pub(crate) struct ShardStats {
    pub(crate) blob_bytes: u64,
    pub(crate) header_pages: u32,
    pub(crate) data_pages: u32,
    pub(crate) page_bytes: u32,
    pub(crate) users: usize,
    pub(crate) user_bytes_avg: f64,
    pub(crate) user_bytes_max: usize,
}

/// 이 공간의 마지막 조각 블롭의 크기(가진 조각만 셈).
pub(crate) fn stats(node: &Node, space: &SpaceId) -> Option<ShardStats> {
    let id = *listed(node, space).last()?;
    let manifest = node.store.blob_manifest(&id).ok()??;
    let first = node.store.blob_read_chunk(&id, 0).ok()??;
    let head: Header = postcard::from_bytes(&first).ok()?;
    let total = manifest.chunks.len() as u32;
    let mut out = ShardStats {
        blob_bytes: manifest.size,
        header_pages: head.header_pages,
        data_pages: total.saturating_sub(head.header_pages),
        page_bytes: PAGE,
        ..ShardStats::default()
    };
    let mut sum = 0usize;
    for i in head.header_pages..total {
        let Some(c) = node.store.blob_read_chunk(&id, i).ok()? else {
            continue;
        };
        let Ok(page) = postcard::from_bytes::<Page>(&c) else {
            continue;
        };
        for e in &page {
            let n = postcard::to_stdvec(e).map_or(0, |v| v.len());
            sum += n;
            out.user_bytes_max = out.user_bytes_max.max(n);
            out.users += 1;
        }
    }
    if out.users > 0 {
        out.user_bytes_avg = sum as f64 / out.users as f64;
    }
    Some(out)
}

// ───────────── 내주는 쪽 ─────────────

/// 요청 기기 `device`에게 조각 블롭을 `bytes`만큼 더 내줘도 되는지(쓰면 예산에서 뺌).
pub(crate) fn allow_serve(node: &Node, device: DeviceId, bytes: u64) -> bool {
    let now = yangpa_core::time::Timestamp::now().as_millis();
    let mut m = node.keygroup.shard_budget.lock();
    if m.len() >= MAX_BUDGETS && !m.contains_key(&device) {
        m.clear();
    }
    m.entry(device)
        .or_insert_with(|| {
            yangpa_sync::policy::TokenBucket::with_burst(SERVE_RATE_BPS, SERVE_BURST, now)
        })
        .try_consume(bytes, now)
        .is_ok()
}

/// 들어온 `GetKeyShard`: 요청 기기가 활성 멤버 기기이고 예산 안이면, 가진 조각 블롭에서 그 기기의 묶음만 돌려줍니다.
pub(crate) fn handle_get_shard(
    node: &Arc<Node>,
    peer: &PeerId,
    space: SpaceId,
    blob: BlobId,
) -> YpResponse {
    use yangpa_core::wire::WireErrorCode;
    let Some((user, device)) = node.peers.identity_of(peer) else {
        return YpResponse::error(WireErrorCode::Unauthenticated, "인증 전");
    };
    let Some(state) = node.space_state(&space) else {
        return YpResponse::error(WireErrorCode::NotFound, "모르는 공간");
    };
    if !state.is_member(&user) || !state.device_belongs(&user, &device) {
        return YpResponse::error(WireErrorCode::NotMember, "멤버가 아님");
    }
    if !is_shard(node, &space, &blob) || crate::checkpoint::is_unconfirmed(node, &space) {
        // 확인 전 체크포인트 상태의 명단으로는 내주지 않습니다.
        return YpResponse::error(WireErrorCode::NotFound, "없는 블롭");
    }
    if !allow_serve(node, device, SHARD_ASK_COST) {
        return YpResponse::Busy {
            retry_after_ms: 5_000,
        };
    }
    // 그 사용자의 다른 기기 몫은 그 기기에게만 열리므로 내주지 않습니다(기기 2대 사용자면 답이 절반, 기기당 받기 목표 5KB 안).
    match user_bundles(node, &blob, &user) {
        Ok(Some(list)) => match postcard::to_stdvec(&for_device(list, &device)) {
            Ok(data) => YpResponse::Chunk { data },
            Err(_) => YpResponse::error(WireErrorCode::Internal, "직렬화 오류"),
        },
        Ok(None) => YpResponse::error(WireErrorCode::NotFound, "그 조각이 없음"),
        Err(_) => YpResponse::error(WireErrorCode::Internal, "저장소 오류"),
    }
}

/// 가진 조각 블롭에서 사용자 `user`의 묶음. 머리나 그 사용자 조각이 없으면 `None`.
fn user_bundles(node: &Node, blob: &BlobId, user: &UserId) -> Result<Option<Vec<Vec<u8>>>> {
    let Some(m) = node.store.blob_manifest(blob)? else {
        return Ok(None);
    };
    let read = |i: u32| node.store.blob_read_chunk(blob, i);
    let Some(first) = read(0)? else {
        return Ok(None);
    };
    let header_pages = header_len(&first, m.chunks.len())?;
    let mut head = first;
    for i in 1..header_pages {
        let Some(c) = read(i)? else { return Ok(None) };
        head.extend(c);
    }
    let Some(index) = page_of(&head, user, m.chunks.len())? else {
        return Ok(Some(Vec::new()));
    };
    let Some(data) = read(index)? else {
        return Ok(None);
    };
    bundles_in(&data, user).map(Some)
}

/// 첫 조각에서 머리 조각 수.
fn header_len(first: &[u8], chunks: usize) -> Result<u32> {
    let (pre, _): (Prefix, _) =
        postcard::take_from_bytes(first).map_err(|_| Error::invalid("조각 머리"))?;
    if pre.v != SHARD_VERSION || pre.header_pages == 0 || pre.header_pages as usize > chunks {
        return Err(Error::invalid("조각 머리"));
    }
    Ok(pre.header_pages)
}

/// 머리에서 사용자 `user`의 자료 조각 번호(그 사용자 몫이 없으면 `None`).
fn page_of(head: &[u8], user: &UserId, chunks: usize) -> Result<Option<u32>> {
    let (h, _): (Header, _) =
        postcard::take_from_bytes(head).map_err(|_| Error::invalid("조각 머리"))?;
    let at = h.firsts.partition_point(|u| u <= user);
    let Some(page) = at.checked_sub(1) else {
        return Ok(None);
    };
    let index = h.header_pages as usize + page;
    if index >= chunks {
        return Err(Error::invalid("조각 번호"));
    }
    Ok(Some(index as u32))
}

/// 자료 조각에서 사용자 `user`의 묶음 인코딩.
fn bundles_in(data: &[u8], user: &UserId) -> Result<Vec<Vec<u8>>> {
    let (p, _): (Page, _) =
        postcard::take_from_bytes(data).map_err(|_| Error::invalid("자료 조각"))?;
    Ok(p.into_iter()
        .filter(|(u, _)| u == user)
        .flat_map(|(_, list)| list)
        .collect())
}

/// 묶음 인코딩 가운데 기기 `device`의 것(인코딩 그대로).
fn for_device(list: Vec<Vec<u8>>, device: &DeviceId) -> Vec<Vec<u8>> {
    list.into_iter()
        .filter(|b| DeviceBundle::decode(b).is_ok_and(|d| d.device == *device))
        .collect()
}

/// 묶음 인코딩 가운데 내 기기 것.
fn mine(list: Vec<Vec<u8>>, me: &DeviceId) -> Vec<DeviceBundle> {
    list.into_iter()
        .filter_map(|b| DeviceBundle::decode(&b).ok())
        .filter(|b| b.device == *me)
        .collect()
}

// ───────────── 받는 쪽 ─────────────

/// 크루 갱신 `seq`의 내 몫을 조각 블롭에서 받아 옵니다(백그라운드). 확정 기기를 먼저, 그다음 다른 멤버에게 묻습니다.
pub(crate) fn fetch(node: &Arc<Node>, space: SpaceId, seq: u64, blob: BlobId) {
    let k = *blob.as_bytes();
    {
        let mut asked = node.keygroup.asked.lock();
        if asked
            .get(&(space, k))
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(20))
        {
            return;
        }
        asked.insert((space, k), std::time::Instant::now());
    }
    node.spawn_detached(move |node| async move {
        // 물은 곳이 모두 실패하면(확정 기기가 잠깐 끊김 등) 막혀 있는 동안 몇 번 더 묻습니다. 같은 순번은 막힐 때 한 번만
        // 부르므로 여기서 끝내면 다음 갱신이나 중계가 올 때까지 멈춥니다.
        for round in 0..FETCH_ROUNDS {
            if round > 0 {
                tokio::time::sleep(FETCH_AGAIN).await;
                if !still_stuck(&node, space, seq).await {
                    return;
                }
            }
            if fetch_round(&node, space, seq, blob).await {
                return;
            }
        }
    });
}

/// 크루 키가 아직 순번 `seq`에서 막혀 있는지. 적용 잠금을 먼저 잡습니다(긴 적용 동안 스레드를 묶지 않게).
async fn still_stuck(node: &Node, space: SpaceId, seq: u64) -> bool {
    let _l = crate::ingest::lock_space(node, space).await;
    let Some(core) = node.spaces.read().get(&space).map(|rt| rt.core.clone()) else {
        return false;
    };
    let crew = KeyGroupId::crew(&space);
    core.lock()
        .keys
        .groups
        .get(&crew)
        .is_some_and(|g| g.stuck == Some(seq))
}

/// [`fetch`]의 한 바퀴: 확정 기기, 그다음 다른 멤버에게 묻습니다. 더 물을 까닭이 없으면 참.
async fn fetch_round(node: &Arc<Node>, space: SpaceId, seq: u64, blob: BlobId) -> bool {
    let Ok(me) = node.me() else { return true };
    let Some(state) = node.space_state(&space) else {
        return true;
    };
    let mut peers: Vec<PeerId> = Vec::new();
    if let Some(p) = node.peers.authed_peer_of_device(&state.committer) {
        peers.push(p);
    }
    for (p, u, d) in node.peers.authed_peers() {
        if peers.len() >= FETCH_PEERS {
            break;
        }
        if d != me.device_id()
            && state.is_member(&u)
            && state.device_belongs(&u, &d)
            && !peers.contains(&p)
        {
            peers.push(p);
        }
    }
    for peer in peers {
        match fetch_from(node, space, blob, peer, &me.user(), &me.device_id()).await {
            Ok(mine) if !mine.is_empty() => {
                let crew = KeyGroupId::crew(&space);
                ingest::offer_keys(node, space, move |m| {
                    for b in mine {
                        m.offer_bundle(crew, seq, b);
                    }
                })
                .await;
                return true;
            }
            // 블롭은 누가 내주든 같으므로 내 몫이 없으면 더 묻지 않습니다.
            Ok(_) => {
                debug!(space = %space.short(), "조각 블롭에 내 몫이 없음");
                return true;
            }
            Err(e) => debug!(space = %space.short(), error = %e, "키 조각 받기 실패"),
        }
    }
    false
}

async fn ask(node: &Node, peer: PeerId, req: YpRequest) -> Result<YpResponse> {
    node.net
        .request(peer, req)
        .await
        .map_err(|e| Error::Network(e.to_string()))
}

async fn chunk(
    node: &Node,
    space: SpaceId,
    blob: BlobId,
    m: &BlobManifest,
    peer: PeerId,
    index: u32,
) -> Result<Vec<u8>> {
    match ask(node, peer, YpRequest::BlobChunk { space, blob, index }).await? {
        YpResponse::Chunk { data } if m.verify_chunk(index, &data) => {
            // 받은 조각은 나도 내줍니다(확정 기기 한 곳에 몰리지 않게).
            let _ = node.store.blob_write_chunk(&blob, index, &data);
            Ok(data)
        }
        _ => Err(Error::Network("조각을 받지 못함".into())),
    }
}

async fn fetch_from(
    node: &Node,
    space: SpaceId,
    blob: BlobId,
    peer: PeerId,
    me_user: &UserId,
    me: &DeviceId,
) -> Result<Vec<DeviceBundle>> {
    // 먼저 내 사용자 몫만 묻습니다(몇 KB). 비었거나 답이 없으면(옛 노드, 블롭이 없는 멤버, 빈 답은 확인할 수 없음)
    // 매니페스트 해시로 확인하는 조각째 받기로 갑니다.
    if let Ok(YpResponse::Chunk { data }) =
        ask(node, peer, YpRequest::GetKeyShard { space, blob }).await
        && let Ok(list) = postcard::from_bytes::<Vec<Vec<u8>>>(&data)
    {
        let got = mine(list, me);
        if !got.is_empty() {
            return Ok(got);
        }
    }
    let m = match ask(node, peer, YpRequest::BlobManifest { space, blob }).await? {
        YpResponse::Manifest(m) if m.id() == blob && m.chunk_size == PAGE => m,
        _ => return Err(Error::Network("매니페스트를 받지 못함".into())),
    };
    if node.store.blob_manifest(&blob)?.is_none() {
        let pb = PersistentBlob::new(m.clone(), SecurityLevel::Normal)?;
        node.store.blob_register(&space, &pb)?;
    }
    remember(node, &space, blob);
    let first = chunk(node, space, blob, &m, peer, 0).await?;
    let header_pages = header_len(&first, m.chunks.len())?;
    let mut head = first;
    for i in 1..header_pages {
        head.extend(chunk(node, space, blob, &m, peer, i).await?);
    }
    let Some(index) = page_of(&head, me_user, m.chunks.len())? else {
        return Ok(Vec::new());
    };
    let data = chunk(node, space, blob, &m, peer, index).await?;
    Ok(mine(bundles_in(&data, me_user)?, me))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `GetKeyShard` 답에는 요청 기기의 묶음만 남습니다(같은 사용자의 다른 기기 몫·깨진 인코딩은 뺌).
    #[test]
    fn shard_answer_keeps_only_the_asking_device() {
        let bundle = |n: u8| DeviceBundle {
            device: DeviceId([n; 32]),
            envelope: vec![n; 40],
            index: u32::from(n),
            proof: vec![[n; 32]; 3],
        };
        let list = vec![
            bundle(1).encode(),
            bundle(2).encode(),
            vec![0xFF; 7],
            bundle(1).encode(),
        ];
        let got = for_device(list, &DeviceId([1; 32]));
        assert_eq!(got.len(), 2);
        assert!(
            got.iter()
                .all(|b| DeviceBundle::decode(b).is_ok_and(|d| d.device == DeviceId([1; 32])))
        );
        assert!(for_device(vec![bundle(2).encode()], &DeviceId([1; 32])).is_empty());
    }
}
