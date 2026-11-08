//! gossip 이웃 고르기: 큰 크루에서 토픽마다 연결해 둘 멤버 기기(protocol.md "실시간 전파").
//!
//! gossipsub 메시는 연결된 구독자 사이에서만 섭니다. 큰 크루에서 연결이 보낼 때·따라잡을 때 생긴 것뿐이면 메시가 비거나
//! 몇 노드에 몰립니다. 그래서 `redial_interval`마다 토픽(크루, 구독한 그룹)마다 연결된 멤버 기기가 [`NEIGHBORS`]보다
//! 적으면 몇 곳에 겁니다.
//!
//! - 후보는 최근 [`RECENT`] 안에 연결에 성공한 적이 있는 멤버 기기의 노드와 소개받은 노드(`locator::intros`).
//!   크루 토픽 이웃이 모자라면 연결된 멤버 한 곳에 멤버 노드 목록(`GetMemberNodes`)을 물어 소개를 채웁니다. 이것이
//!   없으면 초대해 준 멤버만 아는 새 멤버는 후보가 그 멤버뿐이라, 그가 꺼지면 크루에서 떨어집니다. 소개받은 노드는
//!   걸고 인증해 그 기기일 때만 씁니다(`peers::reach`).
//! - 절반은 소금을 친 랑데부 해시(소금 = 최신 체크포인트 ID, 없으면 공간 ID)로: 기기 쌍마다 같은 점수라 서로를 고르기
//!   쉬워 연결이 오래 가고, 체크포인트가 바뀌면 이웃도 바뀝니다. 나머지 절반은 무작위(메시가 고립된 무리로 갈리지 않게).
//! - 노드의 연결이 [`MAX_CONNECTIONS`]개 이상이면 더 걸지 않습니다. 한 바퀴에 [`DIALS_PER_TICK`]곳까지. 걸어 본 기기는
//!   인증될 때까지 다시 걸기처럼 간격을 두 배씩 늘립니다(`PeerDirectory::neighbor_dialed`).

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use rand::seq::SliceRandom;
use tracing::debug;
use yangpa_core::ids::{ChannelId, DeviceId, KeyGroupId, SpaceId, UserId};
use yangpa_core::model::{Permissions, SpaceState};
use yangpa_core::time::Timestamp;
use yangpa_net::{Multiaddr, PeerId};

use crate::node::Node;

/// 토픽마다 연결해 둘 멤버 기기 수(gossipsub `mesh_n_high`).
pub(crate) const NEIGHBORS: usize = 8;
/// 이웃 고르기가 더 걸지 않는 노드 전체 연결 수(Tor 연결은 회로마다 비쌈).
pub(crate) const MAX_CONNECTIONS: usize = 48;
/// 한 바퀴에 새로 걸어 보는 이웃 수.
const DIALS_PER_TICK: usize = 4;
/// 후보로 삼는 기기: 이 안에 연결에 성공한 적이 있는 노드.
pub(crate) const RECENT: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// 이웃 후보 하나(기록된 노드).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub peer: PeerId,
    pub user: UserId,
    pub device: DeviceId,
    pub addrs: Vec<Multiaddr>,
    /// 소개받아 아직 확인하지 않은 노드(저장소가 모르는 PeerId라 연결 이벤트가 인증하지 않음).
    pub introduced: bool,
}

/// 이웃 고르기 한 바퀴의 할 일.
#[derive(Default)]
pub(crate) struct Plan {
    pub dials: Vec<Candidate>,
    /// 멤버 노드 목록을 물을 (공간, 연결된 멤버 노드).
    pub ask: Vec<(SpaceId, PeerId)>,
}

/// 기기 쌍의 랑데부 점수(작을수록 먼저). 두 기기가 같은 값을 얻도록 순서를 맞춥니다.
fn pair_score(salt: &[u8; 32], a: &DeviceId, b: &DeviceId) -> [u8; 32] {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    let mut h = blake3::Hasher::new_derive_key("yangpa/v2/gossip-neighbor");
    h.update(salt);
    h.update(lo.as_bytes());
    h.update(hi.as_bytes());
    *h.finalize().as_bytes()
}

/// 후보에서 `want`곳: 절반은 랑데부 점수 순, 나머지는 무작위. `connected`(이미 연결된 기기)는 빼고 셉니다.
pub(crate) fn pick(
    me: &DeviceId,
    salt: &[u8; 32],
    mut pool: Vec<Candidate>,
    connected: &HashSet<DeviceId>,
    want: usize,
    rng: &mut impl rand::Rng,
) -> Vec<Candidate> {
    pool.retain(|c| c.device != *me && !connected.contains(&c.device));
    pool.sort_by_cached_key(|c| pair_score(salt, me, &c.device));
    pool.dedup_by(|a, b| a.device == b.device);
    let ranked = want.div_ceil(2).min(pool.len());
    let mut rest = pool.split_off(ranked);
    rest.shuffle(rng);
    pool.extend(rest.into_iter().take(want - ranked.min(want)));
    pool.truncate(want);
    pool
}

/// 공간에서 이웃을 둘 토픽: 크루(gossip을 쓰는 크기일 때, `None`)와 구독한 그룹 토픽(그 그룹 채널 하나와 함께). 같은
/// 그룹 채널은 볼 수 있는 사람이 같아 채널 하나로 후보를 가립니다(`topics` 모듈 참고).
fn wanted_topics(
    node: &Node,
    space: &SpaceId,
    state: &Arc<SpaceState>,
) -> Vec<Option<(KeyGroupId, ChannelId)>> {
    let mut out = Vec::new();
    let index = node.sync.index.get(space, state);
    if index.active_count() > node.cfg.push_max_devices {
        out.push(None);
    }
    let mut groups: BTreeMap<KeyGroupId, ChannelId> = BTreeMap::new();
    for (c, ch) in &state.channels {
        if let Some(g) = state.channel_key_group(space, ch)
            && crate::topics::subscribed(node, space, &g)
        {
            groups.entry(g).or_insert(*c);
        }
    }
    out.extend(groups.into_iter().map(Some));
    out
}

/// 한 바퀴: 모자란 토픽마다 이웃 몇 곳에 겁니다(`peers::spawn_redial`과 같은 주기).
pub(crate) fn tick(node: &Arc<Node>) {
    let plan = plan(node);
    let now = std::time::Instant::now();
    for c in &plan.dials {
        node.peers
            .neighbor_dialed(c.device, now, node.cfg.redial_interval);
    }
    for (space, peer) in plan.ask {
        node.spawn_detached(move |node| async move {
            crate::locator::ask_member_nodes(&node, space, peer).await;
        });
    }
    for c in plan.dials {
        node.spawn_detached(move |node| async move {
            if c.introduced {
                if let Some(p) =
                    crate::peers::reach(&node, c.peer, c.addrs, &c.user, &c.device).await
                {
                    crate::sync_service::catch_up_peer(&node, p);
                }
            // 연결되면 `peers::on_connected`가 인증·따라잡기를 하고, 크루 토픽 허용 목록에도 들어갑니다.
            } else if let Err(e) = crate::peers::connect(&node, c.peer, c.addrs).await {
                debug!(peer = %c.peer, error = %e, "gossip 이웃 연결 실패");
            }
        });
    }
}

/// 이번 바퀴에 걸 노드와 멤버 노드 목록을 물을 곳.
pub(crate) fn plan(node: &Arc<Node>) -> Plan {
    let mut out = Plan::default();
    if node.net.is_closed() || node.peers.connected_count() >= MAX_CONNECTIONS {
        return out;
    }
    let Ok(me) = node.me() else { return out };
    let me_dev = me.device_id();
    let rows = match node.store.identified_peers() {
        Ok(r) => r,
        Err(e) => {
            debug!(error = %e, "노드 기록을 읽지 못해 이웃 고르기를 건너뜀");
            return out;
        }
    };
    let since = Timestamp::now().0.saturating_sub(RECENT.as_millis() as u64);
    let known: Vec<Candidate> = rows
        .into_iter()
        .filter(|r| r.last_success.is_some_and(|t| t.0 >= since))
        .filter_map(|r| {
            Some(Candidate {
                peer: PeerId::from_bytes(&r.peer_id).ok()?,
                user: r.user?,
                device: r.device?,
                addrs: r.addrs.iter().filter_map(|a| a.parse().ok()).collect(),
                introduced: false,
            })
        })
        .filter(|c| !c.addrs.is_empty())
        .chain(crate::locator::intros(node).into_iter().filter_map(|i| {
            let (peer, addrs) = i.found()?;
            Some(Candidate {
                peer,
                user: i.user,
                device: i.device,
                addrs,
                introduced: true,
            })
        }))
        .collect();
    let authed = node.peers.authed_peers();
    let now = std::time::Instant::now();
    let mut budget = DIALS_PER_TICK.min(MAX_CONNECTIONS - node.peers.connected_count());
    let mut dialing: HashSet<DeviceId> = HashSet::new();
    let mut rng = rand::thread_rng();
    for space in node.space_ids() {
        if budget == 0 {
            break;
        }
        if crate::policy::is_dormant(node, &space)
            || crate::friends::space_deactivated(node, &space)
        {
            continue;
        }
        let Some(state) = node.space_state(&space) else {
            continue;
        };
        if !state.is_member(&me.user()) {
            continue;
        }
        let salt: [u8; 32] = crate::checkpoint::saved_base(node, &space)
            .map(|b| b.event.0)
            .unwrap_or_else(|| *blake3::hash(space.as_bytes()).as_bytes());
        for topic in wanted_topics(node, &space, &state) {
            if budget == 0 {
                break;
            }
            let eligible = |u: &UserId, d: &DeviceId| {
                state.is_member(u)
                    && state.device_belongs(u, d)
                    && topic.is_none_or(|(_, c)| {
                        state
                            .channel_permissions(u, &c)
                            .contains(Permissions::VIEW_CHANNEL)
                    })
            };
            let connected: HashSet<DeviceId> = authed
                .iter()
                .filter(|(_, u, d)| eligible(u, d))
                .map(|(_, _, d)| *d)
                .chain(dialing.iter().copied())
                .collect();
            if connected.len() >= NEIGHBORS {
                continue;
            }
            if topic.is_none()
                && crate::locator::member_nodes_due(node, &space)
                && let Some(&(p, _, _)) = authed
                    .iter()
                    .filter(|(_, u, d)| eligible(u, d))
                    .collect::<Vec<_>>()
                    .choose(&mut rng)
            {
                out.ask.push((space, *p));
            }
            let pool: Vec<Candidate> = known
                .iter()
                .filter(|c| eligible(&c.user, &c.device) && node.peers.neighbor_due(&c.device, now))
                .cloned()
                .collect();
            let want = (NEIGHBORS - connected.len()).min(budget);
            for c in pick(&me_dev, &salt, pool, &connected, want, &mut rng) {
                if node.peers.is_connected(&c.peer) {
                    continue;
                }
                budget -= 1;
                dialing.insert(c.device);
                out.dials.push(c);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(i: u8) -> Candidate {
        Candidate {
            peer: PeerId::random(),
            user: UserId([i; 32]),
            device: DeviceId::from_bytes([i; 32]),
            addrs: Vec::new(),
            introduced: false,
        }
    }

    #[test]
    fn pick_mixes_rendezvous_and_random_and_skips_connected() {
        let salt = [5u8; 32];
        let me = DeviceId::from_bytes([0; 32]);
        let pool: Vec<Candidate> = (1..=40).map(cand).collect();
        let connected: HashSet<DeviceId> = [DeviceId::from_bytes([1; 32])].into();
        let mut rng = rand::thread_rng();
        let a = pick(&me, &salt, pool.clone(), &connected, 6, &mut rng);
        assert_eq!(a.len(), 6);
        assert!(
            a.iter()
                .all(|c| c.device != me && !connected.contains(&c.device))
        );
        let uniq: HashSet<DeviceId> = a.iter().map(|c| c.device).collect();
        assert_eq!(uniq.len(), 6);
        // 앞 절반(랑데부)은 매번 같고, 소금이 바뀌면 바뀝니다.
        let b = pick(&me, &salt, pool.clone(), &connected, 6, &mut rng);
        assert_eq!(a[..3], b[..3]);
        let other = pick(&me, &[6u8; 32], pool.clone(), &connected, 6, &mut rng);
        assert_ne!(a[..3], other[..3]);
        // 랑데부 점수는 기기 쌍에 대칭입니다(서로를 고르기 쉬움).
        let (x, y) = (DeviceId::from_bytes([3; 32]), DeviceId::from_bytes([9; 32]));
        assert_eq!(pair_score(&salt, &x, &y), pair_score(&salt, &y, &x));
        // 후보가 모자라면 있는 만큼.
        assert_eq!(
            pick(&me, &salt, pool[..2].to_vec(), &connected, 6, &mut rng).len(),
            1
        );
    }
}
