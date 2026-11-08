//! 휘발성 채널 전용 RAM 저장소.
//!
//! - 이벤트는 받은 그대로의 암호문으로만 두고, 세션 키는 잠긴 메모리([`LockedKeyring`])에 둡니다.
//!   암호문이 스왑으로 내려가도 키 없이는 읽을 수 없습니다.
//! - 수명은 이 기기가 **받은 시점** 기준입니다. 만료·퇴장·채널 삭제 시 암호문 버퍼를 0으로 덮어쓰고 해제합니다.
//! - 지난 세대 세션 키는 그 세대 이벤트가 남아 있는 동안, 그리고 더 쓰이지 않게 된 뒤에도 잠깐(유예 시간) 둡니다.
//!   키 갱신 직전에 보낸 메시지가 늦게 오거나 키가 이벤트보다 먼저 와도 풀 수 있게 하기 위해서입니다.
//! - [`VolatileEvent`]는 `Serialize`를 구현하지 않고, 디스크 저장 API는 [`crate::PersistentEvent`]만 받으므로
//!   이 모듈의 데이터가 디스크 경로로 갈 수 없습니다.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use yangpa_core::blob::BlobManifest;
use yangpa_core::body::Secret32;
use yangpa_core::event::{KeyRef, SignedEvent, StreamId};
use yangpa_core::ids::{BlobId, ChannelId, EventId, SpaceId};
use yangpa_crypto::LockedKeyring;
use zeroize::{Zeroize, Zeroizing};

use crate::error::{Result, StoreError};

/// 잠긴 키 보관소 하나의 키 수(한 페이지 = 32바이트 키 128개).
const KEYRING_CHUNK: usize = 128;
/// 이벤트 하나의 대략적인 고정 비용(헤더·색인).
const EVENT_OVERHEAD: usize = 256;
/// 상한 때문에 밀려난 이벤트 알림을 쌓아 두는 최대 개수(`expire`를 오래 안 불러도 메모리가 늘지 않게).
const MAX_PENDING_EVICTIONS: usize = 65_536;

/// RAM 사용 상한.
#[derive(Clone, Debug)]
pub struct VolatileLimits {
    /// 채널당 이벤트 수. 넘으면 가장 오래된(논리 시계가 작은) 것부터 지웁니다.
    pub max_events_per_channel: usize,
    /// 채널당 암호문 바이트. 넘으면 오래된 것부터 지웁니다.
    pub max_bytes_per_channel: usize,
    /// 전체 이벤트 바이트. 채널 안에서 정리해도 넘으면 새 이벤트를 거부합니다(아무것도 지우지 않음).
    pub max_total_bytes: usize,
    /// 전체 첨부 암호문 바이트(RAM 예산). 첨부를 등록할 때 매니페스트 크기만큼 미리 잡으므로, 넘으면 받기를
    /// 시작하기 전에 [`StoreError::LimitExceeded`]로 거절합니다(받는 도중에 모자라지 않게).
    pub max_blob_bytes: usize,
    /// 첨부 하나의 최대 크기(암호문 바이트).
    pub max_blob_size: u64,
    /// 현재 세대가 아니고 그 세대 이벤트도 남지 않은 세션 키를 지우기 전 유예 시간.
    pub key_grace: Duration,
    /// 채널당 세대 키 수. 넘으면 안 쓰는 세대부터, 그래도 넘으면 가장 오래 쓰이지 않은 세대를 그 세대 이벤트와 함께
    /// 밀어냅니다(키 넣기가 늘 짧게 끝나고 키 보관소가 한없이 커지지 않게).
    pub max_generations_per_channel: usize,
}

impl Default for VolatileLimits {
    fn default() -> Self {
        VolatileLimits {
            max_events_per_channel: 10_000,
            max_bytes_per_channel: 64 * 1024 * 1024,
            max_total_bytes: 512 * 1024 * 1024,
            max_blob_bytes: 1024 * 1024 * 1024,
            max_blob_size: 100 * 1024 * 1024,
            key_grace: Duration::from_secs(60),
            max_generations_per_channel: 1024,
        }
    }
}

/// RAM에만 있는 휘발성 이벤트. 직렬화할 수 없고, 해제될 때 암호문 버퍼를 0으로 덮어씁니다.
pub struct VolatileEvent {
    /// 받은 그대로의 서명 이벤트(본문은 세션 키 암호문).
    pub event: SignedEvent,
    pub received_at: Instant,
    /// 수명 만료 시각(받은 시점 + N초). `None`이면 퇴장·채널 삭제 때까지.
    pub expires_at: Option<Instant>,
    id: EventId,
}

impl VolatileEvent {
    /// `ttl`이 너무 커서 시각 계산이 넘치면 수명 없음으로 봅니다(패닉하지 않음).
    pub fn new(event: SignedEvent, received_at: Instant, ttl: Option<Duration>) -> Self {
        let id = event.id();
        let expires_at = ttl.and_then(|t| received_at.checked_add(t));
        VolatileEvent {
            event,
            received_at,
            expires_at,
            id,
        }
    }

    pub fn id(&self) -> EventId {
        self.id
    }

    /// `now` 기준 남은 수명(만료되었으면 0).
    pub fn remaining(&self, now: Instant) -> Option<Duration> {
        self.expires_at.map(|e| e.saturating_duration_since(now))
    }

    fn is_expired(&self, now: Instant) -> bool {
        self.expires_at.is_some_and(|e| e <= now)
    }

    fn generation(&self) -> Option<u64> {
        match self.event.header.key {
            KeyRef::Volatile { generation } => Some(generation),
            _ => None,
        }
    }

    fn cost(&self) -> usize {
        self.event.body.as_ref().map_or(0, Vec::len)
            + self.event.header.parents.len() * 32
            + EVENT_OVERHEAD
    }

    fn order_key(&self) -> (u64, EventId) {
        (self.event.header.lamport, self.id)
    }
}

impl Clone for VolatileEvent {
    fn clone(&self) -> Self {
        VolatileEvent {
            event: self.event.clone(),
            received_at: self.received_at,
            expires_at: self.expires_at,
            id: self.id,
        }
    }
}

impl std::fmt::Debug for VolatileEvent {
    // 내용(암호문)은 찍지 않습니다.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VolatileEvent")
            .field("id", &self.id)
            .field("lamport", &self.event.header.lamport)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl VolatileEvent {
    /// 암호문 버퍼를 0으로 덮어쓰고 비웁니다(해제 시 자동 호출).
    pub(crate) fn wipe(&mut self) {
        if let Some(body) = self.event.body.as_mut() {
            body.zeroize();
        }
    }
}

impl Drop for VolatileEvent {
    fn drop(&mut self) {
        self.wipe();
    }
}

/// 늦은 입장자에게 줄 스냅숏 한 항목. 해제 시 암호문 사본을 0으로 덮어씁니다.
#[derive(Clone)]
pub struct SnapshotItem {
    pub event: SignedEvent,
    /// 남은 수명(`None`이면 수명 없음).
    pub remaining: Option<Duration>,
}

impl std::fmt::Debug for SnapshotItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotItem")
            .field("id", &self.event.id())
            .field("remaining", &self.remaining)
            .finish()
    }
}

impl Drop for SnapshotItem {
    fn drop(&mut self) {
        if let Some(body) = self.event.body.as_mut() {
            body.zeroize();
        }
    }
}

/// 만료(또는 상한 초과로 밀려나) 지운 이벤트.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExpiredEvent {
    pub space: SpaceId,
    pub channel: ChannelId,
    pub id: EventId,
}

/// 메모리 사용 통계(`diag.metrics`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VolatileStats {
    pub channels: usize,
    pub events: usize,
    pub event_bytes: usize,
    pub blobs: usize,
    /// 실제로 RAM에 든 첨부 암호문 바이트.
    pub blob_bytes: usize,
    /// 첨부 예산으로 잡아 둔 바이트. 받는 중인 첨부는 매니페스트 크기 전체, 받기를 쉬는 첨부는 가진 만큼
    /// ([`VolatileStore::trim_blob_reservation`]).
    pub blob_reserved: usize,
    pub keys: usize,
    /// 키 보관소가 모두 메모리 잠금에 성공했는지.
    pub keys_locked: bool,
}

/// RAM 첨부 하나의 상태(내용 없음).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RamBlobInfo {
    /// 이 첨부를 실은 메시지. `None`이면 보내는 쪽이 아직 메시지에 묶지 않음([`VolatileStore::stage_blob`]).
    pub owner: Option<EventId>,
    /// 가진 조각 수와 전체 조각 수.
    pub have: u32,
    pub total: u32,
    /// 암호문 크기(매니페스트).
    pub size: u64,
}

/// RAM 첨부 조각. 첨부는 그것을 실은 메시지 이벤트(주인) 하나에 묶여, 그 이벤트가 어떤 길로든(만료·삭제·상한
/// 밀어내기·퇴장) 지워지면 함께 지워집니다.
struct RamBlob {
    manifest: BlobManifest,
    chunks: Vec<Option<Vec<u8>>>,
    bytes: usize,
    /// 예산으로 잡은 바이트. 받는 중이면 매니페스트 크기, 받기를 쉬면 가진 바이트.
    reserved: usize,
    owner: Option<EventId>,
}

impl RamBlob {
    fn new(manifest: BlobManifest, owner: Option<EventId>) -> Self {
        let n = manifest.chunks.len();
        RamBlob {
            reserved: Self::full(&manifest),
            manifest,
            chunks: (0..n).map(|_| None).collect(),
            bytes: 0,
            owner,
        }
    }

    fn full(manifest: &BlobManifest) -> usize {
        usize::try_from(manifest.size).unwrap_or(usize::MAX)
    }

    fn have(&self) -> u32 {
        u32::try_from(self.chunks.iter().filter(|c| c.is_some()).count()).unwrap_or(u32::MAX)
    }

    fn info(&self) -> RamBlobInfo {
        RamBlobInfo {
            owner: self.owner,
            have: self.have(),
            total: u32::try_from(self.chunks.len()).unwrap_or(u32::MAX),
            size: self.manifest.size,
        }
    }
}

impl Drop for RamBlob {
    fn drop(&mut self) {
        for c in self.chunks.iter_mut().flatten() {
            c.zeroize();
        }
    }
}

#[derive(Default)]
struct ChannelState {
    /// (논리 시계, ID) 순.
    events: BTreeMap<(u64, EventId), VolatileEvent>,
    by_id: HashMap<EventId, u64>,
    bytes: usize,
    /// 세대별 그 세대 키로 암호화된 이벤트 수.
    gen_refs: HashMap<u64, usize>,
    /// 키가 있는 세대 → 마지막으로 필요했던 시각(키를 넣은 시각, 또는 다음 세대로 바뀐 시각).
    generations: BTreeMap<u64, Instant>,
    /// [`VolatileStore::make_current`]로 정한 현재 세대. 없으면 가장 높은 세대가 현재 세대입니다.
    current: Option<u64>,
    blobs: HashMap<BlobId, RamBlob>,
    blob_bytes: usize,
    blob_reserved: usize,
    /// 메시지 이벤트 → 그 메시지에 묶인 첨부.
    owned: HashMap<EventId, Vec<BlobId>>,
}

impl ChannelState {
    fn remove_event(&mut self, key: &(u64, EventId)) -> Option<VolatileEvent> {
        let ev = self.events.remove(key)?;
        self.by_id.remove(&key.1);
        self.bytes = self.bytes.saturating_sub(ev.cost());
        if let Some(g) = ev.generation()
            && let Some(n) = self.gen_refs.get_mut(&g)
        {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.gen_refs.remove(&g);
            }
        }
        // 메시지가 어떤 길로 지워지든 그 첨부도 같이 지웁니다(RamBlob의 Drop이 0으로 덮어씀).
        if let Some(blobs) = self.owned.remove(&key.1) {
            for b in blobs {
                self.drop_blob(&b);
            }
        }
        Some(ev)
    }

    fn drop_blob(&mut self, id: &BlobId) -> bool {
        let Some(b) = self.blobs.remove(id) else {
            return false;
        };
        self.blob_bytes = self.blob_bytes.saturating_sub(b.bytes);
        self.blob_reserved = self.blob_reserved.saturating_sub(b.reserved);
        if let Some(owner) = b.owner
            && let Some(list) = self.owned.get_mut(&owner)
        {
            list.retain(|x| x != id);
            if list.is_empty() {
                self.owned.remove(&owner);
            }
        }
        true
    }

    fn add_blob(&mut self, id: BlobId, blob: RamBlob) {
        self.blob_bytes += blob.bytes;
        self.blob_reserved += blob.reserved;
        if let Some(owner) = blob.owner {
            self.owned.entry(owner).or_default().push(id);
        }
        self.blobs.insert(id, blob);
    }

    fn current_generation(&self) -> Option<u64> {
        self.current
            .filter(|g| self.generations.contains_key(g))
            .or_else(|| self.generations.last_key_value().map(|(g, _)| *g))
    }

    /// 현재 세대가 아니고 그 세대로 암호화된 이벤트도 없는 세대와, 그 키를 지워도 되는 시각.
    fn idle_generations(&self, grace: Duration) -> impl Iterator<Item = (u64, Instant)> + '_ {
        let current = self.current_generation();
        self.generations
            .iter()
            .filter(move |(g, _)| Some(**g) != current && !self.gen_refs.contains_key(g))
            .map(move |(g, t)| (*g, t.checked_add(grace).unwrap_or(*t)))
    }

    /// `now`에 지워도 되는 세대 키.
    fn unused_generations(&self, now: Instant, grace: Duration) -> Vec<u64> {
        self.idle_generations(grace)
            .filter(|(_, due)| *due <= now)
            .map(|(g, _)| g)
            .collect()
    }

    /// 새 이벤트(`new_key`, `cost`)를 넣으려면 밀어내야 할 오래된 이벤트와 그 바이트 합.
    /// 새 이벤트 자신이 가장 먼저 밀려날 차례면 `None`(넣지 않음).
    fn plan_eviction(
        &self,
        new_key: (u64, EventId),
        cost: usize,
        limits: &VolatileLimits,
    ) -> Option<(Vec<(u64, EventId)>, usize)> {
        let mut evict = Vec::new();
        let mut freed = 0usize;
        let mut oldest = self.events.iter();
        while self.events.len() - evict.len() >= limits.max_events_per_channel
            || self.bytes - freed + cost > limits.max_bytes_per_channel
        {
            let (k, ev) = oldest.next()?;
            if new_key < *k {
                return None;
            }
            evict.push(*k);
            freed += ev.cost();
        }
        Some((evict, freed))
    }

    fn is_empty(&self) -> bool {
        self.events.is_empty() && self.blobs.is_empty() && self.generations.is_empty()
    }
}

/// 잠긴 키 보관소 여러 개(보관소 하나는 크기가 고정이라 가득 차면 하나 더 만듦). 빈 보관소는 해제해
/// 잠긴 메모리가 키가 가장 많았던 때 크기로 남지 않게 합니다(빈 자리는 `None`으로 두어 번호를 유지).
#[derive(Default)]
struct KeyPool {
    rings: Vec<Option<LockedKeyring>>,
    slot_ring: HashMap<[u8; 16], usize>,
}

impl KeyPool {
    fn insert(&mut self, id: [u8; 16], key: &Secret32) -> Result<()> {
        if let Some(&i) = self.slot_ring.get(&id)
            && let Some(ring) = self.rings[i].as_mut()
        {
            return Ok(ring.insert(id, key)?);
        }
        let free = self
            .rings
            .iter()
            .position(|r| r.as_ref().is_some_and(|r| r.len() < r.capacity()));
        let idx = match free {
            Some(i) => i,
            None => {
                let ring = LockedKeyring::new(KEYRING_CHUNK)?;
                if !ring.is_locked() {
                    tracing::warn!(
                        "휘발성 키 보관소의 메모리 잠금 실패(OS 한도). 키가 스왑될 수 있음"
                    );
                }
                match self.rings.iter().position(Option::is_none) {
                    Some(i) => {
                        self.rings[i] = Some(ring);
                        i
                    }
                    None => {
                        self.rings.push(Some(ring));
                        self.rings.len() - 1
                    }
                }
            }
        };
        if let Some(ring) = self.rings[idx].as_mut() {
            ring.insert(id, key)?;
        }
        self.slot_ring.insert(id, idx);
        Ok(())
    }

    /// 키 사본(해제 시 0으로 덮어씀). 저장소 잠금을 오래 잡지 않으려고 사본을 꺼냅니다.
    fn copy(&self, id: &[u8; 16]) -> Option<Secret32> {
        let i = *self.slot_ring.get(id)?;
        self.rings[i].as_ref()?.with_key(id, Secret32::clone)
    }

    fn contains(&self, id: &[u8; 16]) -> bool {
        self.slot_ring.contains_key(id)
    }

    fn remove(&mut self, id: &[u8; 16]) -> bool {
        let Some(i) = self.slot_ring.remove(id) else {
            return false;
        };
        let Some(ring) = self.rings[i].as_mut() else {
            return false;
        };
        let removed = ring.remove(id);
        // 빈 보관소는 해제합니다(Drop이 0으로 덮어쓰고 잠금을 풂). 하나는 남겨 넣고 빼기를 오갈 때 다시 만들지 않게.
        if ring.is_empty() && self.rings.iter().flatten().count() > 1 {
            self.rings[i] = None;
        }
        removed
    }

    fn len(&self) -> usize {
        self.slot_ring.len()
    }

    /// 지금 가진(해제하지 않은) 잠긴 보관소 수.
    #[cfg(test)]
    fn ring_count(&self) -> usize {
        self.rings.iter().flatten().count()
    }

    fn all_locked(&self) -> bool {
        self.rings.iter().flatten().all(LockedKeyring::is_locked)
    }
}

/// 세션 키 슬롯 ID. 공간·채널·세대를 해시해 보관소 하나에 여러 채널 키를 섞어 둡니다.
fn key_slot(space: &SpaceId, channel: &ChannelId, generation: u64) -> [u8; 16] {
    let mut h = blake3::Hasher::new();
    h.update(b"yangpa/v1/volatile-key-slot\0");
    h.update(space.as_bytes());
    h.update(channel.as_bytes());
    h.update(&generation.to_be_bytes());
    let mut out = [0u8; 16];
    out.copy_from_slice(&h.finalize().as_bytes()[..16]);
    out
}

/// 키 비교. 일치하는 앞부분 길이가 시간으로 드러나지 않게 끝까지 비교합니다.
fn same_key(a: &Secret32, b: &Secret32) -> bool {
    a.0.iter()
        .zip(b.0.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

#[derive(Default)]
struct Inner {
    channels: HashMap<(SpaceId, ChannelId), ChannelState>,
    keys: KeyPool,
    total_bytes: usize,
    /// 상한 때문에 밀려난 이벤트(밀려난 시각). 다음 `expire`가 만료 목록과 함께 돌려줍니다.
    evicted: VecDeque<(Instant, ExpiredEvent)>,
}

impl Inner {
    /// 모든 채널의 첨부 예산 사용량(채널 수가 적어 그때그때 셉니다).
    fn blob_reserved(&self) -> usize {
        self.channels.values().map(|c| c.blob_reserved).sum()
    }

    fn drop_generation_keys(&mut self, space: &SpaceId, channel: &ChannelId, gens: &[u64]) {
        for g in gens {
            self.keys.remove(&key_slot(space, channel, *g));
        }
        if let Some(ch) = self.channels.get_mut(&(*space, *channel)) {
            for g in gens {
                ch.generations.remove(g);
                if ch.current == Some(*g) {
                    ch.current = None;
                }
            }
        }
    }

    /// 새 세대 키를 넣기 전에 채널의 세대 수를 상한 아래로 맞춥니다. 유예가 지난 안 쓰는 세대를 먼저 지우고, 그래도
    /// 가득 차 있으면 현재 세대가 아닌 것 중 이벤트가 없는 세대, 그다음 가장 오래 쓰이지 않은 세대를 그 세대 이벤트와
    /// 함께 밀어냅니다(키 없이 남은 암호문은 풀 수 없으므로). 밀려난 이벤트는 다음 `expire`가 알려 줍니다.
    /// 상한에 닿았을 때만 세대·이벤트를 한 번 훑으므로 키 넣기 하나의 비용은 상한에 묶입니다.
    fn make_room_for_generation(
        &mut self,
        space: &SpaceId,
        channel: &ChannelId,
        now: Instant,
        limits: &VolatileLimits,
    ) {
        let full = |inner: &Inner| {
            inner
                .channels
                .get(&(*space, *channel))
                .is_some_and(|ch| ch.generations.len() >= limits.max_generations_per_channel)
        };
        if !full(self) {
            return;
        }
        self.gc_keys(space, channel, now, limits.key_grace);
        while full(self) {
            let Some(ch) = self.channels.get_mut(&(*space, *channel)) else {
                return;
            };
            let current = ch.current_generation();
            let victim = ch
                .generations
                .iter()
                .filter(|(g, _)| Some(**g) != current)
                .min_by_key(|(g, t)| (ch.gen_refs.contains_key(g), **t, **g))
                .map(|(g, _)| *g);
            let Some(victim) = victim else {
                return;
            };
            let doomed: Vec<(u64, EventId)> = ch
                .events
                .iter()
                .filter(|(_, e)| e.generation() == Some(victim))
                .map(|(k, _)| *k)
                .collect();
            let mut freed = 0usize;
            for k in &doomed {
                if let Some(ev) = ch.remove_event(k) {
                    freed += ev.cost();
                    if self.evicted.len() >= MAX_PENDING_EVICTIONS {
                        self.evicted.pop_front();
                    }
                    self.evicted.push_back((
                        now,
                        ExpiredEvent {
                            space: *space,
                            channel: *channel,
                            id: k.1,
                        },
                    ));
                }
            }
            self.total_bytes = self.total_bytes.saturating_sub(freed);
            self.drop_generation_keys(space, channel, &[victim]);
        }
    }

    /// 유예 시간이 지난 안 쓰는 세대 키를 지웁니다.
    fn gc_keys(&mut self, space: &SpaceId, channel: &ChannelId, now: Instant, grace: Duration) {
        let gens = match self.channels.get(&(*space, *channel)) {
            Some(ch) => ch.unused_generations(now, grace),
            None => return,
        };
        if !gens.is_empty() {
            self.drop_generation_keys(space, channel, &gens);
        }
    }

    fn drop_channel(&mut self, space: &SpaceId, channel: &ChannelId) -> usize {
        let Some(ch) = self.channels.remove(&(*space, *channel)) else {
            return 0;
        };
        for g in ch.generations.keys() {
            self.keys.remove(&key_slot(space, channel, *g));
        }
        self.total_bytes = self.total_bytes.saturating_sub(ch.bytes);
        // 지운 채널의 밀려남 알림도 버립니다(채널을 지운 쪽이 이미 앎).
        self.evicted
            .retain(|(_, e)| !(e.space == *space && e.channel == *channel));
        // ch가 여기서 해제되며 VolatileEvent·RamBlob의 Drop이 버퍼를 0으로 덮어씁니다.
        ch.events.len()
    }
}

/// 휘발성 채널 RAM 저장소. 여러 스레드에서 공유해 씁니다(`Arc<VolatileStore>`).
pub struct VolatileStore {
    inner: Mutex<Inner>,
    /// 데몬이 시작할 때 설정값으로 바꿉니다([`Self::set_limits`]).
    limits: RwLock<VolatileLimits>,
}

impl Default for VolatileStore {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for VolatileStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "VolatileStore({:?})", self.stats())
    }
}

impl VolatileStore {
    pub fn new() -> Self {
        Self::with_limits(VolatileLimits::default())
    }

    pub fn with_limits(limits: VolatileLimits) -> Self {
        VolatileStore {
            inner: Mutex::new(Inner::default()),
            limits: RwLock::new(limits),
        }
    }

    /// 상한을 바꿉니다. 이미 있는 데이터는 그대로 두고 이후 넣기·등록부터 적용합니다.
    pub fn set_limits(&self, limits: VolatileLimits) {
        *self.limits.write() = limits;
    }

    pub fn limits(&self) -> VolatileLimits {
        self.limits.read().clone()
    }

    /// 이벤트를 넣습니다. `ttl`은 채널 수명(받은 시점 기준).
    ///
    /// 넣었으면 true, 넣지 않았으면 false(이미 있음, 이미 만료, 채널이 가득 찼는데 이 이벤트가 가장 오래돼 바로
    /// 밀려날 차례). 채널 상한을 넘으면 가장 오래된 이벤트부터 밀어내고, 밀려난 이벤트는 다음 [`Self::expire`]가
    /// 만료 목록과 함께 돌려줍니다. 전체 상한을 넘으면 아무것도 지우지 않고 [`StoreError::LimitExceeded`].
    /// 이벤트의 공간·채널 스트림이 맞지 않거나 형식이 틀리면 거부합니다.
    pub fn insert(
        &self,
        space: &SpaceId,
        channel: &ChannelId,
        event: SignedEvent,
        ttl: Option<Duration>,
        now: Instant,
    ) -> Result<bool> {
        if event.header.space != *space || event.header.stream() != StreamId::Channel(*channel) {
            return Err(StoreError::InvalidInput("휘발성 이벤트의 공간·채널 불일치"));
        }
        event
            .header
            .check_shape()
            .map_err(|_| StoreError::InvalidInput("이벤트 헤더 형식"))?;
        if let Some(body) = &event.body
            && (body.len() != event.header.body_len as usize
                || *blake3::hash(body).as_bytes() != event.header.body_hash)
        {
            return Err(StoreError::InvalidInput("본문 해시 불일치"));
        }
        let ev = VolatileEvent::new(event, now, ttl);
        if ev.is_expired(now) {
            return Ok(false);
        }
        let limits = self.limits();
        let cost = ev.cost();
        if cost > limits.max_bytes_per_channel {
            return Err(StoreError::LimitExceeded("휘발성 이벤트 크기"));
        }
        let key = (*space, *channel);
        let new_key = ev.order_key();
        let mut inner = self.inner.lock();
        let inner = &mut *inner;
        // 먼저 계획만 세우고, 전체 상한까지 통과한 뒤에 지웁니다(거부할 때 아무것도 잃지 않게).
        let (evict, freed) = match inner.channels.get(&key) {
            Some(ch) if ch.by_id.contains_key(&ev.id) => return Ok(false),
            Some(ch) => match ch.plan_eviction(new_key, cost, &limits) {
                Some(plan) => plan,
                None => return Ok(false),
            },
            None => (Vec::new(), 0),
        };
        if inner.total_bytes.saturating_sub(freed) + cost > limits.max_total_bytes {
            return Err(StoreError::LimitExceeded("휘발성 저장소 전체 크기"));
        }
        let ch = inner.channels.entry(key).or_default();
        for k in &evict {
            if ch.remove_event(k).is_some() {
                if inner.evicted.len() >= MAX_PENDING_EVICTIONS {
                    inner.evicted.pop_front();
                }
                inner.evicted.push_back((
                    now,
                    ExpiredEvent {
                        space: *space,
                        channel: *channel,
                        id: k.1,
                    },
                ));
            }
        }
        if let Some(g) = ev.generation() {
            *ch.gen_refs.entry(g).or_default() += 1;
        }
        ch.bytes += cost;
        ch.by_id.insert(ev.id, new_key.0);
        ch.events.insert(new_key, ev);
        inner.total_bytes = inner.total_bytes.saturating_sub(freed) + cost;
        if !evict.is_empty() {
            inner.gc_keys(space, channel, now, limits.key_grace);
        }
        Ok(true)
    }

    /// 채널의 이벤트 사본을 (논리 시계, ID) 순으로. 사본도 해제 시 0으로 덮어씁니다.
    pub fn list(&self, space: &SpaceId, channel: &ChannelId) -> Vec<VolatileEvent> {
        let inner = self.inner.lock();
        inner
            .channels
            .get(&(*space, *channel))
            .map(|ch| ch.events.values().cloned().collect())
            .unwrap_or_default()
    }

    pub fn get(&self, space: &SpaceId, channel: &ChannelId, id: &EventId) -> Option<VolatileEvent> {
        let inner = self.inner.lock();
        let ch = inner.channels.get(&(*space, *channel))?;
        let lamport = *ch.by_id.get(id)?;
        ch.events.get(&(lamport, *id)).cloned()
    }

    pub fn contains(&self, space: &SpaceId, channel: &ChannelId, id: &EventId) -> bool {
        let inner = self.inner.lock();
        inner
            .channels
            .get(&(*space, *channel))
            .is_some_and(|ch| ch.by_id.contains_key(id))
    }

    pub fn len(&self, space: &SpaceId, channel: &ChannelId) -> usize {
        let inner = self.inner.lock();
        inner
            .channels
            .get(&(*space, *channel))
            .map_or(0, |ch| ch.events.len())
    }

    /// 이벤트 하나를 지웁니다(휘발성 채널의 메시지 삭제). 있었으면 true.
    /// 그 세대 키는 유예 시간이 지난 뒤 [`Self::expire`]가 정리합니다.
    pub fn remove(&self, space: &SpaceId, channel: &ChannelId, id: &EventId) -> bool {
        let mut inner = self.inner.lock();
        let inner = &mut *inner;
        let Some(ch) = inner.channels.get_mut(&(*space, *channel)) else {
            return false;
        };
        let Some(&lamport) = ch.by_id.get(id) else {
            return false;
        };
        let Some(ev) = ch.remove_event(&(lamport, *id)) else {
            return false;
        };
        inner.total_bytes = inner.total_bytes.saturating_sub(ev.cost());
        true
    }

    /// 수명이 끝난 이벤트를 지우고, 그 사이 상한 때문에 밀려난 이벤트와 함께 돌려줍니다. 유예 시간이 지난
    /// 안 쓰는 세대 키와, 아무것도 남지 않은 채널 기록도 정리합니다. 데몬이 주기적으로(예: 1초) 부릅니다.
    pub fn expire(&self, now: Instant) -> Vec<ExpiredEvent> {
        let mut inner = self.inner.lock();
        let inner = &mut *inner;
        let mut out: Vec<ExpiredEvent> = inner.evicted.drain(..).map(|(_, e)| e).collect();
        let mut freed = 0usize;
        let mut key_gc = Vec::new();
        let limits = self.limits();
        for ((space, channel), ch) in inner.channels.iter_mut() {
            let expired: Vec<(u64, EventId)> = ch
                .events
                .iter()
                .filter(|(_, e)| e.is_expired(now))
                .map(|(k, _)| *k)
                .collect();
            for k in expired {
                if let Some(ev) = ch.remove_event(&k) {
                    freed += ev.cost();
                    out.push(ExpiredEvent {
                        space: *space,
                        channel: *channel,
                        id: k.1,
                    });
                }
            }
            let gens = ch.unused_generations(now, limits.key_grace);
            if !gens.is_empty() {
                key_gc.push((*space, *channel, gens));
            }
        }
        inner.total_bytes = inner.total_bytes.saturating_sub(freed);
        for (space, channel, gens) in key_gc {
            inner.drop_generation_keys(&space, &channel, &gens);
        }
        inner.channels.retain(|_, ch| !ch.is_empty());
        out
    }

    /// 다음에 [`Self::expire`]를 불러야 할 시각(이벤트 만료, 밀려난 이벤트 알림, 세대 키 정리 중 가장 이른 것).
    pub fn next_expiry(&self) -> Option<Instant> {
        let limits = self.limits();
        let inner = self.inner.lock();
        let grace = limits.key_grace;
        let events = inner
            .channels
            .values()
            .flat_map(|ch| ch.events.values())
            .filter_map(|e| e.expires_at);
        let keys = inner
            .channels
            .values()
            .flat_map(|ch| ch.idle_generations(grace).map(|(_, due)| due));
        let evicted = inner.evicted.front().map(|(t, _)| *t);
        events.chain(keys).chain(evicted).min()
    }

    /// 퇴장·채널 삭제: 채널의 이벤트·첨부·키를 모두 지웁니다. 지운 이벤트 수를 돌려줍니다.
    pub fn drop_channel(&self, space: &SpaceId, channel: &ChannelId) -> usize {
        self.inner.lock().drop_channel(space, channel)
    }

    /// 공간을 떠날 때: 그 공간의 모든 휘발성 채널을 지웁니다.
    pub fn drop_space(&self, space: &SpaceId) -> usize {
        let mut inner = self.inner.lock();
        let channels: Vec<ChannelId> = inner
            .channels
            .keys()
            .filter(|(s, _)| s == space)
            .map(|(_, c)| *c)
            .collect();
        channels.iter().map(|c| inner.drop_channel(space, c)).sum()
    }

    /// 데이터(이벤트·첨부·키)가 있는 채널 목록.
    pub fn channels(&self) -> Vec<(SpaceId, ChannelId)> {
        let inner = self.inner.lock();
        let mut v: Vec<_> = inner
            .channels
            .iter()
            .filter(|(_, ch)| !ch.is_empty())
            .map(|(k, _)| *k)
            .collect();
        v.sort();
        v
    }

    // ---- 세션 키 ----

    /// 세대 키를 잠긴 메모리에 넣습니다. 가장 높은 세대가 현재 세대이고, 지난 세대 키는 그 세대 이벤트가 남아
    /// 있는 동안과 그 뒤 유예 시간([`VolatileLimits::key_grace`]) 동안 유지됩니다. 지난 세대 키를 나중에 넣어도
    /// (늦은 입장자 스냅숏 등) 바로 지워지지 않습니다. 안 쓰는 키 정리는 [`Self::expire`]가 하고, 여기서는 채널
    /// 세대 수가 상한([`VolatileLimits::max_generations_per_channel`])에 닿았을 때만 자리를 만듭니다(넣기 하나가
    /// 세대 수에 비례해 느려지지 않게).
    ///
    /// 같은 세대에 이미 다른 키가 있으면 [`StoreError::InvalidInput`]으로 거부합니다(먼저 받은 키 유지). 덮어쓰면
    /// 이미 받은 그 세대 이벤트를 풀 수 없게 되기 때문입니다. 같은 키를 다시 넣는 것은 괜찮습니다.
    pub fn set_key(
        &self,
        space: &SpaceId,
        channel: &ChannelId,
        generation: u64,
        key: &Secret32,
        now: Instant,
    ) -> Result<()> {
        let slot = key_slot(space, channel, generation);
        let mut inner = self.inner.lock();
        let inner = &mut *inner;
        if let Some(existing) = inner.keys.copy(&slot)
            && !same_key(&existing, key)
        {
            return Err(StoreError::InvalidInput("같은 세대에 다른 세션 키"));
        }
        let known = inner
            .channels
            .get(&(*space, *channel))
            .is_some_and(|ch| ch.generations.contains_key(&generation));
        if !known {
            inner.make_room_for_generation(space, channel, now, &self.limits());
        }
        inner.keys.insert(slot, key)?;
        let ch = inner.channels.entry((*space, *channel)).or_default();
        if let Some(prev) = ch.current_generation()
            && prev < generation
        {
            // 방금 현재 세대에서 물러난 키의 유예 시간은 지금부터 셉니다.
            ch.generations.insert(prev, now);
        }
        ch.generations.insert(generation, now);
        Ok(())
    }

    /// 세대 키로 작업합니다. 키 사본은 클로저가 끝나면 0으로 덮어써지고, 클로저가 도는 동안 저장소는 잠겨 있지
    /// 않습니다(복호화가 다른 작업을 막지 않고, 클로저 안에서 이 저장소를 불러도 됨).
    pub fn with_key<R>(
        &self,
        space: &SpaceId,
        channel: &ChannelId,
        generation: u64,
        f: impl FnOnce(&Secret32) -> R,
    ) -> Option<R> {
        let key = self
            .inner
            .lock()
            .keys
            .copy(&key_slot(space, channel, generation))?;
        Some(f(&key))
    }

    pub fn has_key(&self, space: &SpaceId, channel: &ChannelId, generation: u64) -> bool {
        self.inner
            .lock()
            .keys
            .contains(&key_slot(space, channel, generation))
    }

    /// 지금 쓰는(가장 높은) 세대.
    pub fn current_generation(&self, space: &SpaceId, channel: &ChannelId) -> Option<u64> {
        let inner = self.inner.lock();
        inner
            .channels
            .get(&(*space, *channel))
            .and_then(ChannelState::current_generation)
    }

    pub fn remove_key(&self, space: &SpaceId, channel: &ChannelId, generation: u64) -> bool {
        let mut inner = self.inner.lock();
        if let Some(ch) = inner.channels.get_mut(&(*space, *channel)) {
            ch.generations.remove(&generation);
            if ch.current == Some(generation) {
                ch.current = None;
            }
        }
        inner.keys.remove(&key_slot(space, channel, generation))
    }

    /// 이미 넣은 세대 키를 현재 세대로 정합니다(번호 크기와 무관). 세대 번호가 무작위라 가장 높은 번호가 가장
    /// 새 키라는 보장이 없을 때, 데몬이 고른 키를 정리 대상에서 지키려고 씁니다. 물러난 이전 현재 세대의 유예
    /// 시간은 지금부터 셉니다. 그 세대 키가 없으면 false.
    pub fn make_current(
        &self,
        space: &SpaceId,
        channel: &ChannelId,
        generation: u64,
        now: Instant,
    ) -> bool {
        let mut inner = self.inner.lock();
        let Some(ch) = inner.channels.get_mut(&(*space, *channel)) else {
            return false;
        };
        if !ch.generations.contains_key(&generation) {
            return false;
        }
        if let Some(prev) = ch.current_generation()
            && prev != generation
        {
            ch.generations.insert(prev, now);
        }
        ch.current = Some(generation);
        true
    }

    // ---- 늦은 입장자 ----

    /// 수명이 남은 이벤트와 남은 수명(과거 기록 공개 채널의 `VolatileSnapshot` 응답).
    pub fn snapshot(
        &self,
        space: &SpaceId,
        channel: &ChannelId,
        now: Instant,
    ) -> Vec<SnapshotItem> {
        let inner = self.inner.lock();
        let Some(ch) = inner.channels.get(&(*space, *channel)) else {
            return Vec::new();
        };
        ch.events
            .values()
            .filter(|e| !e.is_expired(now))
            .map(|e| SnapshotItem {
                event: e.event.clone(),
                remaining: e.remaining(now),
            })
            .collect()
    }

    // ---- 첨부(RAM 전용) ----
    //
    // 첨부는 그것을 실은 메시지(주인 이벤트)에 묶입니다. 주인이 지워지면(만료·삭제·밀어내기·퇴장) 함께 0으로
    // 덮어쓰고 해제합니다. 예산([`VolatileLimits::max_blob_bytes`])은 등록할 때 매니페스트 크기만큼 잡고, 받기를
    // 쉬는 동안은 가진 만큼으로 줄입니다([`VolatileStore::trim_blob_reservation`]).

    /// 남은 첨부 예산(바이트).
    pub fn blob_room(&self) -> u64 {
        let max = self.limits().max_blob_bytes;
        let used = self.inner.lock().blob_reserved();
        max.saturating_sub(used) as u64
    }

    fn check_blob_size(manifest: &BlobManifest, limits: &VolatileLimits) -> Result<()> {
        manifest
            .check()
            .map_err(|_| StoreError::InvalidInput("블롭 매니페스트"))?;
        if manifest.chunks.is_empty() {
            return Err(StoreError::InvalidInput("블롭 매니페스트"));
        }
        if manifest.size > limits.max_blob_size {
            return Err(StoreError::LimitExceeded("휘발성 첨부 크기"));
        }
        Ok(())
    }

    /// 받을 휘발성 첨부를 등록하고 예산(매니페스트 크기 전체)을 잡습니다. 새로 등록했으면 true, 같은 주인으로
    /// 이미 있으면 false(받기를 쉬느라 줄여 둔 예산은 다시 전체로 잡음).
    ///
    /// 주인 메시지가 이 채널에 없으면 [`StoreError::NotFound`](지워진 메시지의 첨부는 받지 않음), 다른 메시지에
    /// 묶인 첨부면 [`StoreError::InvalidInput`], 크기·예산을 넘으면 [`StoreError::LimitExceeded`].
    pub fn register_blob(
        &self,
        space: &SpaceId,
        channel: &ChannelId,
        owner: &EventId,
        manifest: BlobManifest,
    ) -> Result<bool> {
        let limits = self.limits();
        Self::check_blob_size(&manifest, &limits)?;
        let id = manifest.id();
        let mut inner = self.inner.lock();
        let reserved = inner.blob_reserved();
        let ch = inner
            .channels
            .get_mut(&(*space, *channel))
            .filter(|ch| ch.by_id.contains_key(owner))
            .ok_or(StoreError::NotFound)?;
        let full = RamBlob::full(&manifest);
        if let Some(b) = ch.blobs.get_mut(&id) {
            if b.owner != Some(*owner) {
                return Err(StoreError::InvalidInput("다른 메시지에 묶인 첨부"));
            }
            let more = full.saturating_sub(b.reserved);
            if reserved.saturating_add(more) > limits.max_blob_bytes {
                return Err(StoreError::LimitExceeded("휘발성 첨부 전체 크기"));
            }
            b.reserved += more;
            ch.blob_reserved += more;
            return Ok(false);
        }
        if reserved.saturating_add(full) > limits.max_blob_bytes {
            return Err(StoreError::LimitExceeded("휘발성 첨부 전체 크기"));
        }
        ch.add_blob(id, RamBlob::new(manifest, Some(*owner)));
        Ok(true)
    }

    /// 받기를 쉬는 첨부의 예산을 가진 바이트로 줄입니다(보낸 기기가 나가 있는 동안 다른 첨부의 예산을 붙잡지
    /// 않게). 다시 받기 전에 [`Self::register_blob`]으로 전체를 잡습니다. 그 첨부가 없으면 false.
    pub fn trim_blob_reservation(
        &self,
        space: &SpaceId,
        channel: &ChannelId,
        blob: &BlobId,
    ) -> bool {
        let mut inner = self.inner.lock();
        let Some(ch) = inner.channels.get_mut(&(*space, *channel)) else {
            return false;
        };
        let Some(b) = ch.blobs.get_mut(blob) else {
            return false;
        };
        let less = b.reserved.saturating_sub(b.bytes);
        b.reserved -= less;
        ch.blob_reserved = ch.blob_reserved.saturating_sub(less);
        true
    }

    /// 보내는 쪽: 이미 암호화한 조각 전체를 주인 없이 올립니다. 메시지를 만든 뒤 [`Self::claim_blob`]으로 묶고,
    /// 보내지 못하면 [`Self::remove_staged`]로 지웁니다. 묶기 전에는 시간이 지나도 지우지 않습니다(큰 파일 여러
    /// 개를 천천히 읽는 동안 먼저 올린 것이 사라지지 않게). 지우는 책임은 올린 쪽에 있습니다.
    /// 조각마다 매니페스트 해시를 확인하고, 크기·예산을 넘으면 [`StoreError::LimitExceeded`].
    pub fn stage_blob(
        &self,
        space: &SpaceId,
        channel: &ChannelId,
        manifest: BlobManifest,
        chunks: Vec<Vec<u8>>,
    ) -> Result<BlobId> {
        // 실패해도 넘겨받은 암호문은 해제 전에 덮어씁니다.
        let mut chunks: Vec<Zeroizing<Vec<u8>>> = chunks.into_iter().map(Zeroizing::new).collect();
        let limits = self.limits();
        Self::check_blob_size(&manifest, &limits)?;
        if chunks.len() != manifest.chunks.len()
            || chunks
                .iter()
                .enumerate()
                .any(|(i, c)| !manifest.verify_chunk(i as u32, c))
        {
            return Err(StoreError::InvalidInput("조각 해시 불일치"));
        }
        let id = manifest.id();
        let mut blob = RamBlob::new(manifest, None);
        for (slot, c) in blob.chunks.iter_mut().zip(chunks.iter_mut()) {
            blob.bytes += c.len();
            *slot = Some(std::mem::take(&mut **c));
        }
        let mut inner = self.inner.lock();
        if inner.blob_reserved().saturating_add(blob.reserved) > limits.max_blob_bytes {
            return Err(StoreError::LimitExceeded("휘발성 첨부 전체 크기"));
        }
        let ch = inner.channels.entry((*space, *channel)).or_default();
        if ch.blobs.contains_key(&id) {
            return Err(StoreError::InvalidInput("이미 있는 첨부"));
        }
        ch.add_blob(id, blob);
        Ok(id)
    }

    /// 주인 없이 올린 첨부를 메시지 `owner`에 묶습니다. 묶었거나 이미 그 메시지에 묶여 있으면 true.
    /// 그 메시지가 이 채널에 없거나, 첨부가 없거나, 다른 메시지에 묶여 있으면 false.
    pub fn claim_blob(
        &self,
        space: &SpaceId,
        channel: &ChannelId,
        blob: &BlobId,
        owner: &EventId,
    ) -> bool {
        let mut inner = self.inner.lock();
        let Some(ch) = inner.channels.get_mut(&(*space, *channel)) else {
            return false;
        };
        if !ch.by_id.contains_key(owner) {
            return false;
        }
        let Some(b) = ch.blobs.get_mut(blob) else {
            return false;
        };
        match b.owner {
            Some(o) => o == *owner,
            None => {
                b.owner = Some(*owner);
                ch.owned.entry(*owner).or_default().push(*blob);
                true
            }
        }
    }

    /// 조각을 검증해 RAM에 둡니다. 새로 저장했으면 true, 이미 있으면 false. 예산은 등록 때 잡았습니다.
    pub fn write_blob_chunk(
        &self,
        space: &SpaceId,
        channel: &ChannelId,
        blob: &BlobId,
        index: u32,
        data: &[u8],
    ) -> Result<bool> {
        let mut inner = self.inner.lock();
        let ch = inner
            .channels
            .get_mut(&(*space, *channel))
            .ok_or(StoreError::NotFound)?;
        let b = ch.blobs.get_mut(blob).ok_or(StoreError::NotFound)?;
        if !b.manifest.verify_chunk(index, data) {
            return Err(StoreError::InvalidInput("조각 해시 불일치"));
        }
        let slot = b
            .chunks
            .get_mut(index as usize)
            .ok_or(StoreError::InvalidInput("조각 번호"))?;
        if slot.is_some() {
            return Ok(false);
        }
        *slot = Some(data.to_vec());
        b.bytes += data.len();
        ch.blob_bytes += data.len();
        Ok(true)
    }

    /// 조각 사본. 해제 시 0으로 덮어씁니다.
    pub fn read_blob_chunk(
        &self,
        space: &SpaceId,
        channel: &ChannelId,
        blob: &BlobId,
        index: u32,
    ) -> Option<Zeroizing<Vec<u8>>> {
        let inner = self.inner.lock();
        let b = inner.channels.get(&(*space, *channel))?.blobs.get(blob)?;
        b.chunks
            .get(index as usize)?
            .as_ref()
            .map(|c| Zeroizing::new(c.clone()))
    }

    /// 가진 조각 비트맵(최상위 비트부터)과 전체 조각 수.
    pub fn blob_have(
        &self,
        space: &SpaceId,
        channel: &ChannelId,
        blob: &BlobId,
    ) -> Option<(Vec<u8>, u32)> {
        let inner = self.inner.lock();
        let b = inner.channels.get(&(*space, *channel))?.blobs.get(blob)?;
        let mut bm = vec![0u8; b.chunks.len().div_ceil(8)];
        for (i, c) in b.chunks.iter().enumerate() {
            if c.is_some() {
                bm[i / 8] |= 0x80 >> (i % 8);
            }
        }
        Some((bm, u32::try_from(b.chunks.len()).unwrap_or(u32::MAX)))
    }

    pub fn blob_manifest(
        &self,
        space: &SpaceId,
        channel: &ChannelId,
        blob: &BlobId,
    ) -> Option<BlobManifest> {
        let inner = self.inner.lock();
        let b = inner.channels.get(&(*space, *channel))?.blobs.get(blob)?;
        Some(b.manifest.clone())
    }

    pub fn blob_info(
        &self,
        space: &SpaceId,
        channel: &ChannelId,
        blob: &BlobId,
    ) -> Option<RamBlobInfo> {
        let inner = self.inner.lock();
        let b = inner.channels.get(&(*space, *channel))?.blobs.get(blob)?;
        Some(b.info())
    }

    /// 이 공간에서 그 첨부가 든 휘발성 채널(없으면 `None`).
    pub fn find_blob(&self, space: &SpaceId, blob: &BlobId) -> Option<ChannelId> {
        let inner = self.inner.lock();
        inner
            .channels
            .iter()
            .find(|((s, _), ch)| s == space && ch.blobs.contains_key(blob))
            .map(|((_, c), _)| *c)
    }

    /// RAM에 있는 모든 첨부 ID(`diag.leakcheck`가 디스크 블롭 저장소에 같은 것이 없는지 봄).
    pub fn blob_ids(&self) -> Vec<BlobId> {
        let inner = self.inner.lock();
        inner
            .channels
            .values()
            .flat_map(|ch| ch.blobs.keys().copied())
            .collect()
    }

    pub fn remove_blob(&self, space: &SpaceId, channel: &ChannelId, blob: &BlobId) -> bool {
        let mut inner = self.inner.lock();
        inner
            .channels
            .get_mut(&(*space, *channel))
            .is_some_and(|ch| ch.drop_blob(blob))
    }

    /// 아직 메시지에 묶이지 않은 첨부([`Self::stage_blob`])만 지웁니다. 이미 묶인 것은 그대로 둡니다(메시지와 함께
    /// 지워짐). 지웠으면 true.
    pub fn remove_staged(&self, space: &SpaceId, channel: &ChannelId, blob: &BlobId) -> bool {
        let mut inner = self.inner.lock();
        inner
            .channels
            .get_mut(&(*space, *channel))
            .filter(|ch| ch.blobs.get(blob).is_some_and(|b| b.owner.is_none()))
            .is_some_and(|ch| ch.drop_blob(blob))
    }

    // ---- 통계 ----

    pub fn stats(&self) -> VolatileStats {
        let inner = self.inner.lock();
        VolatileStats {
            channels: inner.channels.values().filter(|c| !c.is_empty()).count(),
            events: inner.channels.values().map(|c| c.events.len()).sum(),
            event_bytes: inner.total_bytes,
            blobs: inner.channels.values().map(|c| c.blobs.len()).sum(),
            blob_bytes: inner.channels.values().map(|c| c.blob_bytes).sum(),
            blob_reserved: inner.blob_reserved(),
            keys: inner.keys.len(),
            keys_locked: inner.keys.all_locked(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Fx;
    use yangpa_core::blob::MIN_CHUNK_SIZE;

    const T: Duration = Duration::from_secs(10);

    #[test]
    fn insert_list_dedup_and_channel_check() {
        let vs = VolatileStore::new();
        let fx = Fx::new();
        let now = Instant::now();
        let b = fx.volatile_msg(1, 2, b"b");
        let a = fx.volatile_msg(1, 1, b"a");
        assert!(
            vs.insert(&fx.space, &fx.channel, b.clone(), None, now)
                .unwrap()
        );
        assert!(
            vs.insert(&fx.space, &fx.channel, a.clone(), None, now)
                .unwrap()
        );
        assert!(
            !vs.insert(&fx.space, &fx.channel, a.clone(), None, now)
                .unwrap()
        );
        let ids: Vec<EventId> = vs
            .list(&fx.space, &fx.channel)
            .iter()
            .map(VolatileEvent::id)
            .collect();
        assert_eq!(ids, vec![a.id(), b.id()], "(논리 시계, ID) 순");
        assert_eq!(vs.get(&fx.space, &fx.channel, &a.id()).unwrap().event, a);
        assert!(vs.contains(&fx.space, &fx.channel, &b.id()));
        assert!(matches!(
            vs.insert(&fx.space, &ChannelId::random(), a.clone(), None, now),
            Err(StoreError::InvalidInput(_))
        ));
        assert!(vs.remove(&fx.space, &fx.channel, &a.id()));
        assert!(!vs.remove(&fx.space, &fx.channel, &a.id()));
        assert_eq!(vs.len(&fx.space, &fx.channel), 1);
        assert_eq!(vs.channels(), vec![(fx.space, fx.channel)]);
    }

    #[test]
    fn expiry_by_receive_time_and_key_gc() {
        let vs = VolatileStore::new();
        let grace = VolatileLimits::default().key_grace;
        let fx = Fx::new();
        let t0 = Instant::now();
        vs.set_key(&fx.space, &fx.channel, 1, &Secret32([1; 32]), t0)
            .unwrap();
        let old = fx.volatile_msg(1, 1, b"old");
        let keep = fx.volatile_msg(1, 2, b"no-ttl");
        assert!(
            vs.insert(&fx.space, &fx.channel, old.clone(), Some(T), t0)
                .unwrap()
        );
        assert!(
            vs.insert(&fx.space, &fx.channel, keep.clone(), None, t0)
                .unwrap()
        );
        // 이미 만료된 수명으로 받은 이벤트는 넣지 않음.
        assert!(
            !vs.insert(
                &fx.space,
                &fx.channel,
                fx.volatile_msg(1, 9, b"x"),
                Some(Duration::ZERO),
                t0
            )
            .unwrap()
        );
        // 세대 2로 갱신: 세대 1 이벤트가 남아 있어 세대 1 키도 유지.
        vs.set_key(&fx.space, &fx.channel, 2, &Secret32([2; 32]), t0)
            .unwrap();
        let late = fx.volatile_msg(2, 3, b"late");
        assert!(
            vs.insert(
                &fx.space,
                &fx.channel,
                late.clone(),
                Some(T),
                t0 + Duration::from_secs(5)
            )
            .unwrap()
        );
        assert_eq!(vs.current_generation(&fx.space, &fx.channel), Some(2));
        assert!(vs.has_key(&fx.space, &fx.channel, 1));
        assert_eq!(vs.next_expiry(), Some(t0 + T));

        assert!(vs.expire(t0 + Duration::from_secs(9)).is_empty());
        let gone = vs.expire(t0 + T);
        assert_eq!(
            gone,
            vec![ExpiredEvent {
                space: fx.space,
                channel: fx.channel,
                id: old.id()
            }]
        );
        // 수명 없는 세대 1 이벤트가 남아 있어 키 유지.
        assert!(vs.has_key(&fx.space, &fx.channel, 1));
        assert!(vs.remove(&fx.space, &fx.channel, &keep.id()));
        assert!(
            vs.has_key(&fx.space, &fx.channel, 1),
            "세대 1 이벤트가 없어져도 유예 시간 동안은 키 유지(늦게 오는 메시지)"
        );
        assert!(vs.has_key(&fx.space, &fx.channel, 2), "현재 세대 키는 유지");
        assert_eq!(
            vs.with_key(&fx.space, &fx.channel, 2, |k| k.0),
            Some([2; 32])
        );

        let gone = vs.expire(t0 + Duration::from_secs(15));
        assert_eq!(
            gone.iter().map(|g| g.id).collect::<Vec<_>>(),
            vec![late.id()]
        );
        assert_eq!(vs.len(&fx.space, &fx.channel), 0);
        assert!(vs.has_key(&fx.space, &fx.channel, 1));
        assert_eq!(vs.next_expiry(), Some(t0 + grace), "다음 할 일은 키 정리");
        assert!(vs.expire(t0 + grace).is_empty());
        assert!(
            !vs.has_key(&fx.space, &fx.channel, 1),
            "유예 시간이 지나면 지난 세대 키 삭제"
        );
        assert!(vs.has_key(&fx.space, &fx.channel, 2));
        assert_eq!(vs.next_expiry(), None);
        assert!(vs.remove_key(&fx.space, &fx.channel, 2));
        assert_eq!(vs.stats().keys, 0);
        assert!(vs.channels().is_empty());
        vs.expire(t0 + grace);
        assert_eq!(vs.stats().channels, 0, "빈 채널 기록 정리");
    }

    #[test]
    fn snapshot_has_remaining_lifetime() {
        let vs = VolatileStore::new();
        let fx = Fx::new();
        let t0 = Instant::now();
        let a = fx.volatile_msg(1, 1, b"a");
        let b = fx.volatile_msg(1, 2, b"b");
        vs.insert(&fx.space, &fx.channel, a.clone(), Some(T), t0)
            .unwrap();
        vs.insert(&fx.space, &fx.channel, b.clone(), None, t0)
            .unwrap();
        let snap = vs.snapshot(&fx.space, &fx.channel, t0 + Duration::from_secs(4));
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].event, a);
        assert_eq!(snap[0].remaining, Some(Duration::from_secs(6)));
        assert_eq!(snap[1].remaining, None);
        // 만료 시각이 지난 것은 빠짐(expire 호출 전이어도).
        let snap = vs.snapshot(&fx.space, &fx.channel, t0 + T);
        assert_eq!(
            snap.iter().map(|s| s.event.id()).collect::<Vec<_>>(),
            vec![b.id()]
        );
        assert!(vs.snapshot(&fx.space, &ChannelId::random(), t0).is_empty());
        assert!(
            !format!("{:?}", snap[0]).contains('['),
            "디버그 출력에 암호문 없음"
        );
    }

    #[test]
    fn drop_channel_wipes_everything() {
        let vs = VolatileStore::new();
        let fx = Fx::new();
        let other = Fx {
            space: fx.space,
            channel: ChannelId::random(),
            dev: fx.dev.clone(),
            user: fx.user.clone(),
        };
        let now = Instant::now();
        vs.set_key(&fx.space, &fx.channel, 7, &Secret32::random(), now)
            .unwrap();
        vs.set_key(&other.space, &other.channel, 1, &Secret32::random(), now)
            .unwrap();
        let mut owner = None;
        for l in 1..=5 {
            let ev = fx.volatile_msg(7, l, &[l as u8; 100]);
            owner.get_or_insert(ev.id());
            vs.insert(&fx.space, &fx.channel, ev, Some(T), now).unwrap();
        }
        vs.insert(
            &other.space,
            &other.channel,
            other.volatile_msg(1, 1, b"o"),
            None,
            now,
        )
        .unwrap();
        let data = vec![5u8; MIN_CHUNK_SIZE as usize];
        let m = BlobManifest::build(&data, MIN_CHUNK_SIZE);
        vs.register_blob(&fx.space, &fx.channel, &owner.unwrap(), m.clone())
            .unwrap();
        vs.write_blob_chunk(&fx.space, &fx.channel, &m.id(), 0, &data)
            .unwrap();
        let st = vs.stats();
        assert_eq!((st.channels, st.events, st.keys, st.blobs), (2, 6, 2, 1));
        assert!(st.event_bytes > 0 && st.blob_bytes == data.len());
        assert_eq!(st.blob_reserved, data.len());

        assert_eq!(vs.drop_channel(&fx.space, &fx.channel), 5);
        assert_eq!(vs.len(&fx.space, &fx.channel), 0);
        assert!(!vs.has_key(&fx.space, &fx.channel, 7));
        assert!(
            vs.read_blob_chunk(&fx.space, &fx.channel, &m.id(), 0)
                .is_none()
        );
        let st = vs.stats();
        assert_eq!(
            (
                st.channels,
                st.events,
                st.keys,
                st.blobs,
                st.blob_bytes,
                st.blob_reserved
            ),
            (1, 1, 1, 0, 0, 0)
        );
        assert!(
            vs.has_key(&other.space, &other.channel, 1),
            "다른 채널은 그대로"
        );
        assert_eq!(vs.drop_space(&fx.space), 1);
        assert_eq!(
            vs.stats(),
            VolatileStats {
                keys_locked: vs.stats().keys_locked,
                ..Default::default()
            }
        );
    }

    #[test]
    fn wipe_zeroizes_ciphertext_buffer() {
        let fx = Fx::new();
        let mut ev = VolatileEvent::new(fx.volatile_msg(1, 1, &[0xAB; 64]), Instant::now(), None);
        let (ptr, cap) = {
            let body = ev.event.body.as_ref().unwrap();
            (body.as_ptr(), body.capacity())
        };
        ev.wipe();
        let body = ev.event.body.as_ref().unwrap();
        assert!(body.is_empty());
        assert_eq!(
            body.capacity(),
            cap,
            "버퍼를 해제하지 않고 그 자리에서 덮어씀"
        );
        // SAFETY: Vec가 같은 할당을 계속 소유하고(용량 동일), zeroize가 용량 전체를 0으로 채워 초기화돼 있습니다.
        let raw = unsafe { std::slice::from_raw_parts(ptr, cap) };
        assert!(raw.iter().all(|b| *b == 0));
    }

    #[test]
    fn limits_evict_oldest_and_reject_overflow() {
        let limits = VolatileLimits {
            max_events_per_channel: 3,
            max_bytes_per_channel: 10_000,
            max_total_bytes: 2_000,
            ..Default::default()
        };
        let vs = VolatileStore::with_limits(limits);
        let fx = Fx::new();
        let now = Instant::now();
        let evs: Vec<SignedEvent> = (1..=4).map(|l| fx.volatile_msg(1, l, &[1; 10])).collect();
        for e in &evs {
            vs.insert(&fx.space, &fx.channel, e.clone(), None, now)
                .unwrap();
        }
        let ids: Vec<EventId> = vs
            .list(&fx.space, &fx.channel)
            .iter()
            .map(VolatileEvent::id)
            .collect();
        assert_eq!(
            ids,
            evs[1..].iter().map(SignedEvent::id).collect::<Vec<_>>(),
            "가장 오래된 것부터 밀려남"
        );
        // 밀려난 이벤트는 다음 expire가 알려 줌(데몬이 message.expired로 전달).
        assert_eq!(vs.next_expiry(), Some(now));
        assert_eq!(
            vs.expire(now),
            vec![ExpiredEvent {
                space: fx.space,
                channel: fx.channel,
                id: evs[0].id()
            }]
        );
        assert!(vs.expire(now).is_empty(), "한 번만 알림");
        // 전체 상한을 넘는 다른 채널 이벤트는 거부.
        let other = Fx {
            space: fx.space,
            channel: ChannelId::random(),
            dev: fx.dev.clone(),
            user: fx.user.clone(),
        };
        let big = other.volatile_msg(1, 1, &[0; 1500]);
        assert!(matches!(
            vs.insert(&other.space, &other.channel, big, None, now),
            Err(StoreError::LimitExceeded(_))
        ));
        let huge = fx.volatile_msg(1, 9, &[0; 20_000]);
        assert!(matches!(
            vs.insert(&fx.space, &fx.channel, huge, None, now),
            Err(StoreError::LimitExceeded(_))
        ));
    }

    #[test]
    fn ram_blobs_verify_chunks() {
        let vs = VolatileStore::new();
        let fx = Fx::new();
        let now = Instant::now();
        let owner = fx.volatile_msg(1, 1, b"m");
        vs.insert(&fx.space, &fx.channel, owner.clone(), None, now)
            .unwrap();
        let data: Vec<u8> = (0..MIN_CHUNK_SIZE as usize * 2 + 10)
            .map(|i| (i % 251) as u8)
            .collect();
        let m = BlobManifest::build(&data, MIN_CHUNK_SIZE);
        let id = m.id();
        // 주인 메시지가 없으면 등록하지 않음(지워진 메시지의 첨부는 받지 않음).
        assert!(matches!(
            vs.register_blob(
                &fx.space,
                &fx.channel,
                &EventId::from_bytes([9; 32]),
                m.clone()
            ),
            Err(StoreError::NotFound)
        ));
        assert!(
            vs.register_blob(&fx.space, &fx.channel, &owner.id(), m.clone())
                .unwrap()
        );
        assert!(
            !vs.register_blob(&fx.space, &fx.channel, &owner.id(), m.clone())
                .unwrap()
        );
        assert_eq!(vs.find_blob(&fx.space, &id), Some(fx.channel));
        assert_eq!(vs.find_blob(&SpaceId::random(), &id), None);
        let c1 = &data[MIN_CHUNK_SIZE as usize..MIN_CHUNK_SIZE as usize * 2];
        assert!(
            vs.write_blob_chunk(&fx.space, &fx.channel, &id, 0, c1)
                .is_err()
        );
        assert!(
            vs.write_blob_chunk(&fx.space, &fx.channel, &id, 1, c1)
                .unwrap()
        );
        assert!(
            !vs.write_blob_chunk(&fx.space, &fx.channel, &id, 1, c1)
                .unwrap()
        );
        assert_eq!(
            vs.blob_have(&fx.space, &fx.channel, &id),
            Some((vec![0b0100_0000], 3))
        );
        assert_eq!(
            vs.blob_info(&fx.space, &fx.channel, &id),
            Some(RamBlobInfo {
                owner: Some(owner.id()),
                have: 1,
                total: 3,
                size: data.len() as u64,
            })
        );
        assert_eq!(
            vs.read_blob_chunk(&fx.space, &fx.channel, &id, 1)
                .as_deref()
                .map(Vec::as_slice),
            Some(c1)
        );
        assert!(
            vs.write_blob_chunk(&fx.space, &fx.channel, &id, 99, c1)
                .is_err()
        );
        assert_eq!(
            vs.stats().blob_reserved,
            data.len(),
            "다 받기 전에도 예산은 전체 크기"
        );
        assert!(vs.remove_blob(&fx.space, &fx.channel, &id));
        assert_eq!(vs.stats().blob_bytes, 0);
        assert_eq!(vs.stats().blob_reserved, 0);
    }

    /// 메시지가 어떤 길로 지워지든(삭제·만료·상한 밀어내기) 그 첨부도 함께 지워집니다.
    #[test]
    fn blobs_follow_their_message() {
        let vs = VolatileStore::with_limits(VolatileLimits {
            max_events_per_channel: 2,
            ..Default::default()
        });
        let fx = Fx::new();
        let t0 = Instant::now();
        let data = vec![7u8; MIN_CHUNK_SIZE as usize];
        let blob_for = |n: u8| BlobManifest::build(&[n; 100], MIN_CHUNK_SIZE);
        // 삭제.
        let a = fx.volatile_msg(1, 1, b"a");
        vs.insert(&fx.space, &fx.channel, a.clone(), None, t0)
            .unwrap();
        let ma = BlobManifest::build(&data, MIN_CHUNK_SIZE);
        vs.register_blob(&fx.space, &fx.channel, &a.id(), ma.clone())
            .unwrap();
        vs.write_blob_chunk(&fx.space, &fx.channel, &ma.id(), 0, &data)
            .unwrap();
        assert!(vs.remove(&fx.space, &fx.channel, &a.id()));
        assert!(vs.blob_info(&fx.space, &fx.channel, &ma.id()).is_none());
        assert_eq!((vs.stats().blobs, vs.stats().blob_bytes), (0, 0));
        // 만료.
        let b = fx.volatile_msg(1, 2, b"b");
        vs.insert(&fx.space, &fx.channel, b.clone(), Some(T), t0)
            .unwrap();
        vs.register_blob(&fx.space, &fx.channel, &b.id(), blob_for(1))
            .unwrap();
        assert_eq!(vs.stats().blobs, 1);
        vs.expire(t0 + T);
        assert_eq!((vs.stats().blobs, vs.stats().blob_reserved), (0, 0));
        // 상한 밀어내기(채널당 이벤트 2개).
        let c = fx.volatile_msg(1, 3, b"c");
        vs.insert(&fx.space, &fx.channel, c.clone(), None, t0)
            .unwrap();
        vs.register_blob(&fx.space, &fx.channel, &c.id(), blob_for(2))
            .unwrap();
        for l in 4..=5 {
            vs.insert(
                &fx.space,
                &fx.channel,
                fx.volatile_msg(1, l, b"x"),
                None,
                t0,
            )
            .unwrap();
        }
        assert!(!vs.contains(&fx.space, &fx.channel, &c.id()));
        assert_eq!((vs.stats().blobs, vs.stats().blob_reserved), (0, 0));
    }

    #[test]
    fn staged_blob_waits_for_claim_however_long() {
        let vs = VolatileStore::new();
        let fx = Fx::new();
        let t0 = Instant::now();
        let ct: Vec<u8> = (0..MIN_CHUNK_SIZE as usize + 5).map(|i| i as u8).collect();
        let m = BlobManifest::build(&ct, MIN_CHUNK_SIZE);
        let chunks: Vec<Vec<u8>> = ct
            .chunks(MIN_CHUNK_SIZE as usize)
            .map(<[u8]>::to_vec)
            .collect();
        // 조각이 매니페스트와 다르면 올리지 않음.
        let mut bad = chunks.clone();
        bad[1][0] ^= 1;
        assert!(
            vs.stage_blob(&fx.space, &fx.channel, m.clone(), bad)
                .is_err()
        );
        let id = vs
            .stage_blob(&fx.space, &fx.channel, m.clone(), chunks.clone())
            .unwrap();
        let info = vs.blob_info(&fx.space, &fx.channel, &id).unwrap();
        assert_eq!((info.owner, info.have, info.total), (None, 2, 2));
        // 묶기 전에는 시간으로 지우지 않음(큰 파일 여러 개를 천천히 읽는 동안 먼저 올린 첨부가 사라지면 받을 수 없는
        // 첨부가 실린 메시지가 나감). 지우는 것은 올린 쪽(`remove_staged`)의 몫.
        assert_eq!(vs.next_expiry(), None);
        vs.expire(t0 + Duration::from_secs(24 * 3600));
        assert!(vs.blob_info(&fx.space, &fx.channel, &id).is_some());
        // 아직 없는 메시지에는 묶지 못함.
        let ev = fx.volatile_msg(1, 1, b"m");
        assert!(!vs.claim_blob(&fx.space, &fx.channel, &id, &ev.id()));
        vs.insert(&fx.space, &fx.channel, ev.clone(), None, t0)
            .unwrap();
        assert!(vs.claim_blob(&fx.space, &fx.channel, &id, &ev.id()));
        assert!(
            vs.claim_blob(&fx.space, &fx.channel, &id, &ev.id()),
            "같은 주인은 다시 묶어도 됨"
        );
        // 묶인 첨부는 `remove_staged`로 지워지지 않고, 메시지와 함께 지워짐.
        assert!(!vs.remove_staged(&fx.space, &fx.channel, &id));
        vs.expire(t0 + Duration::from_secs(60));
        assert!(vs.blob_info(&fx.space, &fx.channel, &id).is_some());
        // 다른 메시지가 남의 첨부를 자기 것으로 등록하지 못함.
        let other = fx.volatile_msg(1, 2, b"o");
        vs.insert(&fx.space, &fx.channel, other.clone(), None, t0)
            .unwrap();
        assert!(matches!(
            vs.register_blob(&fx.space, &fx.channel, &other.id(), m.clone()),
            Err(StoreError::InvalidInput(_))
        ));
        assert!(!vs.claim_blob(&fx.space, &fx.channel, &id, &other.id()));
        assert!(vs.remove(&fx.space, &fx.channel, &ev.id()));
        assert!(vs.blob_info(&fx.space, &fx.channel, &id).is_none());

        // 보내지 못한 첨부는 올린 쪽이 지움.
        let id2 = vs.stage_blob(&fx.space, &fx.channel, m, chunks).unwrap();
        assert!(vs.stats().blob_reserved > 0);
        assert!(vs.remove_staged(&fx.space, &fx.channel, &id2));
        assert!(!vs.remove_staged(&fx.space, &fx.channel, &id2));
        assert!(vs.blob_info(&fx.space, &fx.channel, &id2).is_none());
        assert_eq!(vs.stats().blob_reserved, 0);
    }

    #[test]
    fn idle_partial_blob_holds_only_what_it_has() {
        let vs = VolatileStore::with_limits(VolatileLimits {
            max_blob_bytes: 6 * MIN_CHUNK_SIZE as usize,
            ..Default::default()
        });
        let fx = Fx::new();
        let now = Instant::now();
        let owner = fx.volatile_msg(1, 1, b"m");
        vs.insert(&fx.space, &fx.channel, owner.clone(), None, now)
            .unwrap();
        let data: Vec<u8> = (0..4 * MIN_CHUNK_SIZE as usize).map(|i| i as u8).collect();
        let m = BlobManifest::build(&data, MIN_CHUNK_SIZE);
        let id = m.id();
        assert!(
            vs.register_blob(&fx.space, &fx.channel, &owner.id(), m.clone())
                .unwrap()
        );
        let c0 = &data[..MIN_CHUNK_SIZE as usize];
        vs.write_blob_chunk(&fx.space, &fx.channel, &id, 0, c0)
            .unwrap();
        assert_eq!(vs.stats().blob_reserved, data.len());
        // 받기를 쉬는 동안은 가진 조각만큼만 잡음: 다른 첨부가 남은 예산을 쓸 수 있음.
        assert!(vs.trim_blob_reservation(&fx.space, &fx.channel, &id));
        assert_eq!(vs.stats().blob_reserved, c0.len());
        assert_eq!(vs.blob_room(), 5 * MIN_CHUNK_SIZE as u64);
        let other = BlobManifest::build(&vec![7u8; 4 * MIN_CHUNK_SIZE as usize], MIN_CHUNK_SIZE);
        vs.register_blob(&fx.space, &fx.channel, &owner.id(), other.clone())
            .unwrap();
        // 다시 받으려면 전체를 잡아야 하는데 남은 예산이 모자람: 가진 조각은 그대로.
        assert!(matches!(
            vs.register_blob(&fx.space, &fx.channel, &owner.id(), m.clone()),
            Err(StoreError::LimitExceeded(_))
        ));
        assert_eq!(
            vs.blob_info(&fx.space, &fx.channel, &id).map(|i| i.have),
            Some(1)
        );
        assert!(vs.remove_blob(&fx.space, &fx.channel, &other.id()));
        assert!(
            !vs.register_blob(&fx.space, &fx.channel, &owner.id(), m)
                .unwrap()
        );
        assert_eq!(vs.stats().blob_reserved, data.len());
        // 다 받은 첨부는 줄일 것이 없음.
        for i in 1..4u32 {
            let c = &data[(i * MIN_CHUNK_SIZE) as usize..((i + 1) * MIN_CHUNK_SIZE) as usize];
            vs.write_blob_chunk(&fx.space, &fx.channel, &id, i, c)
                .unwrap();
        }
        assert!(vs.trim_blob_reservation(&fx.space, &fx.channel, &id));
        assert_eq!(vs.stats().blob_reserved, data.len());
        assert!(!vs.trim_blob_reservation(&fx.space, &fx.channel, &BlobId([1; 32])));
        assert!(vs.remove(&fx.space, &fx.channel, &owner.id()));
        assert_eq!((vs.stats().blob_reserved, vs.stats().blob_bytes), (0, 0));
    }

    #[test]
    fn blob_budget_is_reserved_up_front() {
        let vs = VolatileStore::with_limits(VolatileLimits {
            max_blob_size: 3 * MIN_CHUNK_SIZE as u64,
            max_blob_bytes: 4 * MIN_CHUNK_SIZE as usize,
            ..Default::default()
        });
        let fx = Fx::new();
        let now = Instant::now();
        let owner = fx.volatile_msg(1, 1, b"m");
        vs.insert(&fx.space, &fx.channel, owner.clone(), None, now)
            .unwrap();
        let big = BlobManifest::build(&vec![1u8; 4 * MIN_CHUNK_SIZE as usize], MIN_CHUNK_SIZE);
        assert!(matches!(
            vs.register_blob(&fx.space, &fx.channel, &owner.id(), big),
            Err(StoreError::LimitExceeded(_))
        ));
        let three = BlobManifest::build(&vec![2u8; 3 * MIN_CHUNK_SIZE as usize], MIN_CHUNK_SIZE);
        vs.register_blob(&fx.space, &fx.channel, &owner.id(), three)
            .unwrap();
        assert_eq!(vs.blob_room(), MIN_CHUNK_SIZE as u64);
        // 조각을 하나도 받지 않았어도 예산을 잡아 두므로, 남은 예산을 넘는 두 번째 첨부는 거절.
        let two = BlobManifest::build(&vec![3u8; 2 * MIN_CHUNK_SIZE as usize], MIN_CHUNK_SIZE);
        assert!(matches!(
            vs.register_blob(&fx.space, &fx.channel, &owner.id(), two.clone()),
            Err(StoreError::LimitExceeded(_))
        ));
        let ct = vec![3u8; 2 * MIN_CHUNK_SIZE as usize];
        let chunks = ct
            .chunks(MIN_CHUNK_SIZE as usize)
            .map(<[u8]>::to_vec)
            .collect();
        assert!(matches!(
            vs.stage_blob(&fx.space, &fx.channel, two, chunks),
            Err(StoreError::LimitExceeded(_))
        ));
        // 상한은 실행 중에 바꿀 수 있음(이후 등록부터).
        vs.set_limits(VolatileLimits {
            max_blob_size: 3 * MIN_CHUNK_SIZE as u64,
            max_blob_bytes: 8 * MIN_CHUNK_SIZE as usize,
            ..Default::default()
        });
        assert_eq!(vs.blob_room(), 5 * MIN_CHUNK_SIZE as u64);
    }

    #[test]
    fn total_limit_rejection_evicts_nothing_and_oldest_is_refused() {
        // 비용 = 본문 + 256(부모 없음).
        let limits = VolatileLimits {
            max_events_per_channel: 2,
            max_bytes_per_channel: 10_000,
            max_total_bytes: 1_000,
            ..Default::default()
        };
        let vs = VolatileStore::with_limits(limits);
        let fx = Fx::new();
        let other = Fx {
            space: fx.space,
            channel: ChannelId::random(),
            dev: fx.dev.clone(),
            user: fx.user.clone(),
        };
        let now = Instant::now();
        let a1 = fx.volatile_msg(1, 1, &[1; 10]);
        let a2 = fx.volatile_msg(1, 2, &[2; 10]);
        vs.insert(&fx.space, &fx.channel, a1.clone(), None, now)
            .unwrap();
        vs.insert(&fx.space, &fx.channel, a2.clone(), None, now)
            .unwrap();
        vs.insert(
            &other.space,
            &other.channel,
            other.volatile_msg(1, 1, &[3; 200]),
            None,
            now,
        )
        .unwrap();
        assert_eq!(vs.stats().event_bytes, 266 * 2 + 456);

        // a1을 밀어내도 전체 상한을 넘음: 거부하고 아무것도 지우지 않음.
        assert!(matches!(
            vs.insert(
                &fx.space,
                &fx.channel,
                fx.volatile_msg(1, 3, &[4; 300]),
                None,
                now
            ),
            Err(StoreError::LimitExceeded(_))
        ));
        assert_eq!(vs.len(&fx.space, &fx.channel), 2);
        assert!(vs.contains(&fx.space, &fx.channel, &a1.id()));
        assert_eq!(vs.stats().event_bytes, 266 * 2 + 456);
        assert!(vs.expire(now).is_empty(), "밀려난 이벤트 없음");

        // 같은 크기면 a1을 밀어내고 들어감.
        let a3 = fx.volatile_msg(1, 3, &[5; 10]);
        assert!(
            vs.insert(&fx.space, &fx.channel, a3.clone(), None, now)
                .unwrap()
        );
        // 채널이 가득 찼는데 새 이벤트가 가장 오래됨: 넣자마자 밀려날 것이므로 넣지 않고 아무것도 지우지 않음.
        assert!(
            !vs.insert(
                &fx.space,
                &fx.channel,
                fx.volatile_msg(1, 0, b"older"),
                None,
                now
            )
            .unwrap()
        );
        let ids: Vec<EventId> = vs
            .list(&fx.space, &fx.channel)
            .iter()
            .map(VolatileEvent::id)
            .collect();
        assert_eq!(ids, vec![a2.id(), a3.id()]);
        assert_eq!(
            vs.expire(now).iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![a1.id()]
        );
        assert_eq!(vs.stats().event_bytes, 266 * 2 + 456);
    }

    #[test]
    fn old_generation_keys_survive_rotation_and_late_set() {
        let grace = Duration::from_secs(5);
        let vs = VolatileStore::with_limits(VolatileLimits {
            key_grace: grace,
            ..Default::default()
        });
        let fx = Fx::new();
        let (s, c) = (fx.space, fx.channel);
        let t0 = Instant::now();
        let sec = Duration::from_secs;
        vs.set_key(&s, &c, 1, &Secret32([1; 32]), t0).unwrap();
        vs.set_key(&s, &c, 2, &Secret32([2; 32]), t0 + sec(1))
            .unwrap();
        // 갱신 직전에 보낸 세대 1 메시지가 이벤트보다 늦게 와도 풀 수 있어야 함.
        assert!(vs.has_key(&s, &c, 1), "세대가 바뀌어도 바로 지우지 않음");
        assert_eq!(vs.next_expiry(), Some(t0 + sec(1) + grace));
        let late = fx.volatile_msg(1, 1, b"late");
        vs.insert(&s, &c, late.clone(), None, t0 + sec(2)).unwrap();
        assert!(vs.expire(t0 + sec(100)).is_empty());
        assert_eq!(
            vs.with_key(&s, &c, 1, |k| k.0),
            Some([1; 32]),
            "그 세대 이벤트가 남아 있는 동안 유지"
        );
        assert!(vs.remove(&s, &c, &late.id()));
        vs.expire(t0 + sec(100));
        assert!(!vs.has_key(&s, &c, 1));
        assert!(vs.has_key(&s, &c, 2));

        // 늦은 입장자: 현재 세대 키를 받은 뒤 스냅숏을 풀 지난 세대 키를 받음.
        vs.set_key(&s, &c, 1, &Secret32([1; 32]), t0 + sec(200))
            .unwrap();
        assert_eq!(
            vs.current_generation(&s, &c),
            Some(2),
            "지난 세대를 넣어도 현재 세대는 그대로"
        );
        assert!(vs.has_key(&s, &c, 1), "지난 세대 키를 넣자마자 지우지 않음");
        assert!(vs.expire(t0 + sec(204)).is_empty());
        assert!(vs.has_key(&s, &c, 1));
        vs.expire(t0 + sec(205));
        assert!(!vs.has_key(&s, &c, 1), "쓰는 이벤트가 없으면 유예 뒤 삭제");
        assert!(vs.has_key(&s, &c, 2), "현재 세대는 유예와 무관하게 유지");
    }

    #[test]
    fn explicit_current_generation_survives_gc() {
        let grace = Duration::from_secs(5);
        let vs = VolatileStore::with_limits(VolatileLimits {
            key_grace: grace,
            ..Default::default()
        });
        let fx = Fx::new();
        let (s, c) = (fx.space, fx.channel);
        let t0 = Instant::now();
        let sec = Duration::from_secs;
        vs.set_key(&s, &c, 900, &Secret32([9; 32]), t0).unwrap();
        // 무작위 세대 번호: 새 키(100)가 이전 키(900)보다 작을 수 있음.
        vs.set_key(&s, &c, 100, &Secret32([1; 32]), t0).unwrap();
        assert_eq!(
            vs.current_generation(&s, &c),
            Some(900),
            "정하기 전에는 가장 높은 번호"
        );
        assert!(vs.make_current(&s, &c, 100, t0 + sec(1)));
        assert!(!vs.make_current(&s, &c, 7, t0), "없는 세대");
        assert_eq!(vs.current_generation(&s, &c), Some(100));
        // 이전 현재 세대(900)는 물러난 때부터 유예 시간 뒤 정리되고, 정한 현재 세대는 남음.
        assert!(vs.expire(t0 + sec(5)).is_empty());
        assert!(vs.has_key(&s, &c, 900));
        vs.expire(t0 + sec(1) + grace);
        assert!(!vs.has_key(&s, &c, 900));
        assert!(vs.has_key(&s, &c, 100), "현재 세대는 번호가 작아도 유지");
        // 더 높은 번호를 넣어도 정한 현재 세대는 그대로.
        vs.set_key(&s, &c, 5000, &Secret32([5; 32]), t0 + sec(10))
            .unwrap();
        assert_eq!(vs.current_generation(&s, &c), Some(100));
        assert!(vs.remove_key(&s, &c, 100));
        assert_eq!(
            vs.current_generation(&s, &c),
            Some(5000),
            "지우면 가장 높은 번호로"
        );
    }

    #[test]
    fn generation_cap_evicts_idle_then_oldest_with_events() {
        let vs = VolatileStore::with_limits(VolatileLimits {
            max_generations_per_channel: 4,
            key_grace: Duration::from_secs(3600),
            ..Default::default()
        });
        let fx = Fx::new();
        let (s, c) = (fx.space, fx.channel);
        let t0 = Instant::now();
        let at = |g: u64| t0 + Duration::from_secs(g);
        for g in 1..=4 {
            vs.set_key(&s, &c, g, &Secret32([g as u8; 32]), at(g))
                .unwrap();
        }
        assert!(vs.make_current(&s, &c, 4, at(4)));
        let e2 = fx.volatile_msg(2, 1, b"g2");
        let e3 = fx.volatile_msg(3, 2, b"g3");
        vs.insert(&s, &c, e2.clone(), None, at(4)).unwrap();
        vs.insert(&s, &c, e3.clone(), None, at(4)).unwrap();

        // 가득 참: 유예 전이어도 이벤트 없는 가장 오래된 세대(1)부터 밀려남.
        vs.set_key(&s, &c, 5, &Secret32([5; 32]), at(5)).unwrap();
        assert!(!vs.has_key(&s, &c, 1));
        assert!(vs.has_key(&s, &c, 2) && vs.has_key(&s, &c, 3) && vs.has_key(&s, &c, 4));
        vs.insert(&s, &c, fx.volatile_msg(5, 3, b"g5"), None, at(5))
            .unwrap();
        assert!(vs.expire(at(5)).is_empty(), "이벤트는 아직 잃지 않음");

        // 안 쓰는 세대가 없으면 가장 오래 쓰이지 않은 세대(2)를 그 이벤트와 함께 밀어내고 알림.
        vs.set_key(&s, &c, 6, &Secret32([6; 32]), at(6)).unwrap();
        assert!(!vs.has_key(&s, &c, 2));
        assert!(vs.has_key(&s, &c, 4), "현재 세대는 밀어내지 않음");
        assert!(!vs.contains(&s, &c, &e2.id()));
        assert!(vs.contains(&s, &c, &e3.id()));
        assert_eq!(
            vs.expire(at(6)).iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![e2.id()]
        );
        // 이미 있는 세대를 다시 넣는 것은 자리를 만들지 않음.
        vs.set_key(&s, &c, 3, &Secret32([3; 32]), at(7)).unwrap();
        assert!(vs.has_key(&s, &c, 5) && vs.has_key(&s, &c, 6));
        assert_eq!(vs.stats().keys, 4);
    }

    #[test]
    fn many_generations_stay_capped_and_keyrings_are_freed() {
        let vs = VolatileStore::with_limits(VolatileLimits {
            max_generations_per_channel: 300,
            ..Default::default()
        });
        let fx = Fx::new();
        let now = Instant::now();
        for g in 0..2_000u64 {
            vs.set_key(&fx.space, &fx.channel, g, &Secret32::random(), now)
                .unwrap();
        }
        assert_eq!(vs.stats().keys, 300, "세대 수 상한");
        assert_eq!(
            vs.inner.lock().keys.ring_count(),
            300usize.div_ceil(KEYRING_CHUNK)
        );
        vs.drop_channel(&fx.space, &fx.channel);
        assert_eq!(vs.stats().keys, 0);
        assert_eq!(
            vs.inner.lock().keys.ring_count(),
            1,
            "빈 잠긴 보관소는 하나만 남기고 해제"
        );
        // 해제한 자리를 다시 씀.
        for g in 0..200u64 {
            vs.set_key(&fx.space, &fx.channel, g, &Secret32::random(), now)
                .unwrap();
        }
        assert_eq!(vs.inner.lock().keys.ring_count(), 2);
        assert_eq!(vs.inner.lock().keys.rings.len(), 3);
    }

    #[test]
    fn huge_ttl_does_not_panic() {
        let vs = VolatileStore::new();
        let fx = Fx::new();
        let now = Instant::now();
        let e = fx.volatile_msg(1, 1, b"a");
        assert!(
            vs.insert(&fx.space, &fx.channel, e, Some(Duration::MAX), now)
                .unwrap()
        );
        let snap = vs.snapshot(&fx.space, &fx.channel, now);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].remaining, None, "넘치는 수명은 수명 없음으로");
        assert_eq!(vs.next_expiry(), None);
    }

    #[test]
    fn with_key_runs_without_holding_the_lock() {
        let vs = VolatileStore::new();
        let fx = Fx::new();
        vs.set_key(
            &fx.space,
            &fx.channel,
            1,
            &Secret32([3; 32]),
            Instant::now(),
        )
        .unwrap();
        // 클로저 안에서 저장소를 다시 불러도 교착되지 않아야 함.
        let r = vs.with_key(&fx.space, &fx.channel, 1, |k| {
            (k.0, vs.has_key(&fx.space, &fx.channel, 1), vs.stats().keys)
        });
        assert_eq!(r, Some(([3; 32], true, 1)));
        assert_eq!(vs.with_key(&fx.space, &fx.channel, 9, |_| ()), None);
    }

    #[test]
    fn same_generation_key_is_first_wins() {
        let vs = VolatileStore::new();
        let fx = Fx::new();
        let now = Instant::now();
        vs.set_key(&fx.space, &fx.channel, 4, &Secret32([4; 32]), now)
            .unwrap();
        // 같은 키를 다시 받는 것은 괜찮음(중복 전달).
        vs.set_key(&fx.space, &fx.channel, 4, &Secret32([4; 32]), now)
            .unwrap();
        // 다른 키로 덮어쓰면 이미 받은 그 세대 이벤트를 못 풀게 되므로 거부.
        assert!(matches!(
            vs.set_key(&fx.space, &fx.channel, 4, &Secret32([5; 32]), now),
            Err(StoreError::InvalidInput(_))
        ));
        assert_eq!(
            vs.with_key(&fx.space, &fx.channel, 4, |k| k.0),
            Some([4; 32])
        );
        // 키를 지운 뒤에는(퇴장 후 재입장 등) 새 키를 받을 수 있음.
        vs.drop_channel(&fx.space, &fx.channel);
        vs.set_key(&fx.space, &fx.channel, 4, &Secret32([5; 32]), now)
            .unwrap();
        assert_eq!(
            vs.with_key(&fx.space, &fx.channel, 4, |k| k.0),
            Some([5; 32])
        );
        assert!(same_key(&Secret32([1; 32]), &Secret32([1; 32])));
        let mut other = [1; 32];
        other[31] = 2;
        assert!(!same_key(&Secret32([1; 32]), &Secret32(other)));
    }

    #[test]
    fn rejects_tampered_or_misplaced_events() {
        let vs = VolatileStore::new();
        let fx = Fx::new();
        let now = Instant::now();
        let bad = |f: &dyn Fn(&mut SignedEvent)| {
            let mut e = fx.volatile_msg(1, 1, b"body");
            f(&mut e);
            vs.insert(&fx.space, &fx.channel, e, None, now)
        };
        // 같은 길이 다른 내용, 다른 길이.
        assert!(matches!(
            bad(&|e| e.body = Some(b"evil".to_vec())),
            Err(StoreError::InvalidInput(_))
        ));
        assert!(matches!(
            bad(&|e| e.body = Some(b"longer body".to_vec())),
            Err(StoreError::InvalidInput(_))
        ));
        // 채널 스트림이 아닌 이벤트, 다른 공간, 지원하지 않는 버전.
        assert!(matches!(
            bad(&|e| e.header.channel = None),
            Err(StoreError::InvalidInput(_))
        ));
        assert!(matches!(
            bad(&|e| e.header.space = SpaceId::random()),
            Err(StoreError::InvalidInput(_))
        ));
        assert!(matches!(
            bad(&|e| e.header.v = 99),
            Err(StoreError::InvalidInput(_))
        ));
        // 본문 없는 헤더만은 받음(본문은 나중에 올 수 있음).
        assert!(bad(&|e| e.body = None).unwrap());
        assert_eq!(vs.len(&fx.space, &fx.channel), 1);
    }

    #[test]
    fn empty_channels_are_forgotten() {
        let vs = VolatileStore::new();
        let fx = Fx::new();
        let t0 = Instant::now();
        vs.insert(
            &fx.space,
            &fx.channel,
            fx.volatile_msg(1, 1, b"a"),
            Some(T),
            t0,
        )
        .unwrap();
        assert_eq!(vs.stats().channels, 1);
        assert_eq!(vs.expire(t0 + T).len(), 1);
        assert_eq!(vs.stats().channels, 0);
        assert!(vs.channels().is_empty());
    }

    // `VolatileEvent`가 `Serialize`를 구현하지 않는다는 것을 확인합니다(구현하면 이 검사가 true가 됨).
    struct Probe<T>(std::marker::PhantomData<T>);
    trait NotSerialize {
        fn is_serialize(&self) -> bool {
            false
        }
    }
    impl<T> NotSerialize for Probe<T> {}
    impl<T: serde::Serialize> Probe<T> {
        #[allow(dead_code)]
        fn is_serialize(&self) -> bool {
            true
        }
    }

    #[test]
    fn volatile_event_is_not_serialize() {
        assert!(!Probe::<VolatileEvent>(std::marker::PhantomData).is_serialize());
        assert!(!Probe::<VolatileStore>(std::marker::PhantomData).is_serialize());
        assert!(!Probe::<SnapshotItem>(std::marker::PhantomData).is_serialize());
        // 검사 자체가 동작하는지: 직렬화 가능한 타입은 true.
        assert!(Probe::<SignedEvent>(std::marker::PhantomData).is_serialize());
    }
}
