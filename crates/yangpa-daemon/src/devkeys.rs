//! 기기 키 표: 키 약속(`key_commit`)으로 찾는 기기 공개키(상자 키·서명 키).
//!
//! 공간 상태의 기기 기록([`DeviceRecord`])은 키 대신 약속만 듭니다. 봉인할 때는 기록의 약속으로 이 표에서 상자 키를
//! 찾습니다. 표는 내용 주소라 같은 기기 ID에 다른 상자 키를 실은 위조 목록이 와도 다른 약속의 행에 들어가고, 상태가
//! 가리키는 진짜 약속의 행을 덮지 못합니다. 그래서 봉인은 늘 상태(해소기가 서명·이어받기를 확인한 목록)가 정한 키로
//! 갑니다.
//!
//! 행은 받은 상태 이벤트를 저장할 때 본문의 목록에서 같은 트랜잭션으로 채웁니다([`body_keys`]). 행은 (약속, 공간)마다
//! 하나라, 같은 기기가 있는 다른 공간을 지워도 이 공간이 쓰는 행은 남습니다. DB 앞에 작은 LRU를 둡니다(확정 기기가 큰
//! 크루 전체에 봉인할 때는 한 번에 묶어 읽음).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use yangpa_core::body::EventBody;
use yangpa_core::device::{
    BoxPublicKey, DeviceKeys, DeviceRecord, DeviceSignKeys, SignedDeviceList,
};
use yangpa_core::event::SignedEvent;
use yangpa_core::ids::{DeviceId, SpaceId, UserId};
use yangpa_core::model::SpaceState;

use crate::node::Node;

/// LRU 항목 수. 상자 키 1.2KB + 서명 키 0.9KB라 대략 9MB 이하입니다.
const LRU_CAP: usize = 4096;
/// 서명 키를 물은 기기를 다시 묻기까지([`DeviceKeyTable::start_asking`]).
const ASK_AGAIN: Duration = Duration::from_secs(30);
/// 물은 기기 기록 수 상한(넘으면 오래된 것부터 버림).
const ASKED_CAP: usize = 4096;

#[derive(Default)]
struct Lru {
    map: HashMap<[u8; 32], (Arc<DeviceKeys>, u64)>,
    tick: u64,
}

impl Lru {
    fn get(&mut self, k: &[u8; 32]) -> Option<Arc<DeviceKeys>> {
        self.tick += 1;
        let t = self.tick;
        self.map.get_mut(k).map(|e| {
            e.1 = t;
            e.0.clone()
        })
    }

    fn put(&mut self, k: [u8; 32], v: Arc<DeviceKeys>) {
        if self.map.len() >= LRU_CAP && !self.map.contains_key(&k) {
            // 가득 차면 오래 안 쓴 절반을 한꺼번에 버립니다(한 번에 O(n), 넣을 때마다는 O(1)).
            let mut ticks: Vec<u64> = self.map.values().map(|e| e.1).collect();
            let mid = ticks.len() / 2;
            let (_, cut, _) = ticks.select_nth_unstable(mid);
            let cut = *cut;
            self.map.retain(|_, e| e.1 > cut);
        }
        self.tick += 1;
        self.map.insert(k, (v, self.tick));
    }
}

/// 노드의 기기 키 표(DB `device_keys` 앞의 LRU).
#[derive(Default)]
pub(crate) struct DeviceKeyTable {
    lru: Mutex<Lru>,
    /// 짧은 서명 확인용 키를 최근에 피어에게 물은 기기.
    asked: Mutex<HashMap<(SpaceId, DeviceId), Instant>>,
}

impl DeviceKeyTable {
    /// 약속들의 키(못 찾은 약속은 빠짐). LRU에 없는 것은 DB에서 한 번에 읽습니다.
    pub(crate) fn get_many(
        &self,
        node: &Node,
        commits: &[[u8; 32]],
    ) -> HashMap<[u8; 32], Arc<DeviceKeys>> {
        let mut out = HashMap::with_capacity(commits.len());
        let mut missing = Vec::new();
        {
            let mut lru = self.lru.lock();
            for c in commits {
                match lru.get(c) {
                    Some(k) => {
                        out.insert(*c, k);
                    }
                    None => missing.push(*c),
                }
            }
        }
        if missing.is_empty() {
            return out;
        }
        missing.sort_unstable();
        missing.dedup();
        match node.store.device_keys(&missing) {
            Ok(found) => {
                // 큰 묶음(크루 전체 봉인)은 LRU를 밀어내지 않게 넣지 않습니다.
                let cache = found.len() <= LRU_CAP / 4;
                let mut lru = self.lru.lock();
                for (c, k) in found {
                    let k = Arc::new(k);
                    if cache {
                        lru.put(c, k.clone());
                    }
                    out.insert(c, k);
                }
            }
            Err(e) => tracing::warn!(error = %e, "기기 키 표 읽기 실패"),
        }
        out
    }

    fn remember(&self, keys: &[DeviceKeys]) {
        let mut lru = self.lru.lock();
        for k in keys {
            lru.put(k.key_commit(), Arc::new(k.clone()));
        }
    }

    /// `devices` 가운데 최근에 묻지 않은 것을 물은 것으로 적고 돌려줍니다(같은 기기를 짧은 간격으로 거듭 묻지 않게).
    pub(crate) fn start_asking(&self, space: SpaceId, devices: &[DeviceId]) -> Vec<DeviceId> {
        let now = Instant::now();
        let mut asked = self.asked.lock();
        if asked.len() >= ASKED_CAP {
            asked.retain(|_, t| now.duration_since(*t) < ASK_AGAIN);
            if asked.len() >= ASKED_CAP {
                asked.clear();
            }
        }
        let mut out = Vec::new();
        for d in devices {
            let fresh = asked
                .get(&(space, *d))
                .is_some_and(|t| now.duration_since(*t) < ASK_AGAIN);
            if !fresh && !out.contains(d) {
                asked.insert((space, *d), now);
                out.push(*d);
            }
        }
        out
    }

    /// 최근에 서명 키를 물은 기기 수(시험용).
    pub(crate) fn asked_count(&self) -> usize {
        self.asked.lock().len()
    }

    /// 모두 비웁니다(이 기기의 데이터를 지울 때).
    pub(crate) fn clear(&self) {
        *self.lru.lock() = Lru::default();
        self.asked.lock().clear();
    }
}

/// 상태 이벤트 본문의 서명된 기기 목록에 실린 키(약속 하나에 하나). 받은 이벤트를 저장할 때 같은 트랜잭션으로 표에
/// 넣습니다(`ingest`). 해소기보다 먼저 넣으므로 적용된 기록이 가리키는 약속의 행은 늘 있습니다.
///
/// 목록 서명은 여기서 보지 않습니다(해소기가 봄). 표는 내용 주소라 서명이 틀린 목록의 키가 들어가도 다른 약속의 행이
/// 될 뿐이고, 봉인은 해소기가 받아들인 기록의 약속으로만 찾습니다. 같은 바이트가 이벤트로도 저장되므로 행이 느는 양도
/// 이벤트를 받는 양(작성 기기 서명·상태 키·속도 제한을 거친 것)에 묶입니다.
pub(crate) fn body_keys(body: &EventBody) -> Vec<DeviceKeys> {
    let lists: Vec<&SignedDeviceList> = match body {
        EventBody::DeviceListUpdate { devices, .. } | EventBody::MemberJoin { devices, .. } => {
            vec![devices]
        }
        EventBody::SpaceCreate { devices, .. } => devices.iter().collect(),
        _ => return Vec::new(),
    };
    let mut out: Vec<DeviceKeys> = lists
        .into_iter()
        .flat_map(|l| l.list.devices.iter().map(|d| d.keys()))
        .collect();
    out.sort_by_cached_key(DeviceKeys::key_commit);
    out.dedup_by(|a, b| a == b);
    out
}

/// 기록의 상자 키. 표에 없으면 그 사용자의 저장된 서명 목록(`device_lists`)에서 약속이 같은 항목을 찾아 표에도
/// 넣습니다(같은 약속이면 같은 키라 어디서 찾든 같음).
fn box_key_with_fallback(
    node: &Node,
    user: &UserId,
    rec: &DeviceRecord,
    found: &HashMap<[u8; 32], Arc<DeviceKeys>>,
) -> Option<BoxPublicKey> {
    if let Some(k) = found.get(&rec.key_commit) {
        return Some(k.box_key.clone());
    }
    let list = node.store.get_device_list(user).ok()??;
    let e = list
        .list
        .devices
        .iter()
        .find(|e| e.device == rec.device && e.key_commit() == rec.key_commit)?;
    node.devkeys.remember(&[e.keys()]);
    Some(e.box_key.clone())
}

/// 기록들의 봉인 받을 곳(기기 ID, 상자 키). 키를 찾지 못한 기기는 빠집니다(로그에는 기기 ID 앞부분만).
pub(crate) fn sealing_keys<'a>(
    node: &Node,
    recs: impl IntoIterator<Item = (&'a UserId, &'a DeviceRecord)>,
) -> Vec<(DeviceId, BoxPublicKey)> {
    sealing_keys_with_user(node, recs)
        .into_iter()
        .map(|(_, d, k)| (d, k))
        .collect()
}

/// [`sealing_keys`]에 사용자를 붙인 것.
pub(crate) fn sealing_keys_with_user<'a>(
    node: &Node,
    recs: impl IntoIterator<Item = (&'a UserId, &'a DeviceRecord)>,
) -> Vec<(UserId, DeviceId, BoxPublicKey)> {
    let recs: Vec<(&UserId, &DeviceRecord)> = recs.into_iter().collect();
    let commits: Vec<[u8; 32]> = recs.iter().map(|(_, r)| r.key_commit).collect();
    let found = node.devkeys.get_many(node, &commits);
    recs.into_iter()
        .filter_map(|(u, r)| match box_key_with_fallback(node, u, r, &found) {
            Some(k) => Some((*u, r.device, k)),
            None => {
                tracing::debug!(device = %r.device.short(), "기기 키를 찾지 못해 봉인에서 뺌");
                None
            }
        })
        .collect()
}

/// 한 사용자의 활성 기기에 봉인할 곳.
pub(crate) fn user_sealing_keys(
    node: &Node,
    state: &SpaceState,
    user: &UserId,
) -> Vec<(DeviceId, BoxPublicKey)> {
    match state.devices.get(user) {
        Some(l) => sealing_keys(node, l.active().map(|d| (user, d))),
        None => Vec::new(),
    }
}

/// 사용자의 활성 기기 하나의 상자 키(그 기기가 지금 그 멤버의 활성 기기일 때만).
pub(crate) fn device_box_key(
    node: &Node,
    state: &SpaceState,
    user: &UserId,
    device: &DeviceId,
) -> Option<BoxPublicKey> {
    if !state.is_member(user) {
        return None;
    }
    let r = state
        .devices
        .get(user)?
        .active()
        .find(|r| r.device == *device)?;
    let found = node.devkeys.get_many(node, &[r.key_commit]);
    box_key_with_fallback(node, user, r, &found)
}

/// 사용자의 활성 기기 하나의 서명 공개키(짧은 서명 확인용, [`yangpa_core::sign::Sig::is_slim`]). 상자 키처럼 상태
/// 기록의 약속으로만 찾으므로 남이 넣은 행이 쓰이지 않습니다. 표에도 저장된 목록에도 없으면 `None`(피어에게 물음).
pub(crate) fn device_sign_keys(
    node: &Node,
    state: &SpaceState,
    user: &UserId,
    device: &DeviceId,
    revoked_too: bool,
) -> Option<DeviceSignKeys> {
    if !state.is_member(user) {
        return None;
    }
    let r = state
        .devices
        .get(user)?
        .get(device)
        .filter(|r| revoked_too || r.revoked_at.is_none())?;
    if let Some(k) = node
        .devkeys
        .get_many(node, &[r.key_commit])
        .get(&r.key_commit)
    {
        return Some(k.sign_keys.clone());
    }
    let list = node.store.get_device_list(user).ok()??;
    let e = list
        .list
        .devices
        .iter()
        .find(|e| e.device == r.device && e.key_commit() == r.key_commit)?;
    node.devkeys.remember(&[e.keys()]);
    Some(e.sign_keys.clone())
}

/// 이벤트 서명 확인. 짧은 서명은 상태 기록의 작성 기기 키로 보고, 키를 모르면 거부합니다(받은 쪽이 다시 받음).
pub(crate) fn verify_event(node: &Node, state: &SpaceState, ev: &SignedEvent) -> bool {
    if !ev.sig.is_slim() {
        return ev.verify().is_ok();
    }
    device_sign_keys(node, state, &ev.header.user, &ev.header.author, false)
        .is_some_and(|k| ev.verify_with(&k).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lru_keeps_recent_entries_and_stays_bounded() {
        let mut lru = Lru::default();
        let keys = Arc::new(DeviceKeys {
            device: DeviceId([1; 32]),
            box_key: BoxPublicKey::filled(1),
            sign_keys: yangpa_core::device::DeviceSignKeys::filled(1),
        });
        let k = |i: usize| {
            let mut b = [0u8; 32];
            b[..8].copy_from_slice(&(i as u64).to_le_bytes());
            b
        };
        for i in 0..LRU_CAP {
            lru.put(k(i), keys.clone());
        }
        // 처음 넣은 것을 다시 쓰면 넘칠 때 살아남음.
        assert!(lru.get(&k(0)).is_some());
        lru.put(k(LRU_CAP), keys.clone());
        assert!(lru.map.len() <= LRU_CAP);
        assert!(lru.get(&k(0)).is_some());
        assert!(lru.get(&k(1)).is_none());
        assert!(lru.get(&k(LRU_CAP)).is_some());
    }
}
