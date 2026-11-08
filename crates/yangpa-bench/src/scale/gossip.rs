//! gossipsub 메시 그래프 모형.
//!
//! 노드 N개가 저마다 피어 k개와 이어져 있고(무작위 짝짓기, 같은 짝·자기 연결은 버림) 그중 비율 f가 토픽을
//! 구독할 때, 메시가 생기는지와 글 하나가 구독자에게 닿는 시간을 어림합니다. 실제 설정(`yangpa-net`의
//! `build_config`)의 값을 그대로 씁니다: 하트비트 1초, mesh_n 6, mesh_n_low 4, mesh_n_high 8, gossip_lazy 6,
//! history_gossip 3, flood_publish 켬.
//!
//! 모형의 가정(보수적인 쪽):
//! - 구독한 이웃이 mesh_n_low(4)보다 적은 노드는 메시가 없다고 봅니다. 실제 gossipsub는 있는 만큼 붙이므로
//!   전달은 이 모형보다 조금 낫습니다.
//! - 메시 간선은 메시가 있는 노드끼리만, 무작위 순서로 mesh_n까지 붙이고 받는 쪽은 mesh_n_high까지 받습니다.
//! - 메시로 못 받은 구독자는 받은 이웃의 IHAVE(하트비트마다 메시 밖 구독 이웃 중 6개, 3번)로 IWANT를 거쳐
//!   받습니다(한 홉 지연 3번).
//! - 작성자에게 구독한 이웃이 하나도 없으면 발행이 `NoPeers`로 실패하고 데몬이 모든 기기에 직접 보냅니다
//!   (`sync_service`). 이때 전달은 직접 한 홉으로 셉니다(Tor 회로 만들기 시간은 뺌).
//! - 홉 지연은 로그 정규 분포(중앙값·p95로 정함)이고 간선마다 따로 뽑습니다.
//!
//! 토픽을 아는 변형은 구독자가 연결 자리의 절반을 다른 구독자에게 씁니다(구독자가 적은 토픽에서 메시가
//! 생기려면 무엇이 필요한지 보려는 것).

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::time::Instant;

use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use serde::Serialize;

/// 실제 gossipsub 설정과 같은 값.
pub const MESH_N: usize = 6;
pub const MESH_N_LOW: usize = 4;
pub const MESH_N_HIGH: usize = 8;
pub const GOSSIP_LAZY: usize = 6;
pub const HISTORY_GOSSIP: usize = 3;
pub const HEARTBEAT_MS: f64 = 1_000.0;

/// 홉 하나의 지연(로그 정규). p95/중앙값 비로 퍼짐을 정합니다.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct HopLatency {
    pub median_ms: f64,
    pub p95_ms: f64,
}

impl HopLatency {
    /// Tor 회로 위 한 홉으로 잡은 기본값(중앙값 250ms, p95 1초).
    pub const TOR: HopLatency = HopLatency {
        median_ms: 250.0,
        p95_ms: 1_000.0,
    };

    fn sample(&self, rng: &mut StdRng) -> f64 {
        // 표준 정규 분포의 95% 분위 = 1.6449.
        let sigma = (self.p95_ms / self.median_ms).max(1.0).ln() / 1.644_853_6;
        // 상자-뮬러. u1은 (0, 1]이라 로그가 터지지 않습니다.
        let u1 = 1.0 - rng.r#gen::<f64>();
        let u2 = rng.r#gen::<f64>();
        let z = (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos();
        self.median_ms * (sigma * z).exp()
    }
}

/// 한 칸의 설정.
#[derive(Debug, Clone, Serialize)]
pub struct GossipSpec {
    pub nodes: usize,
    pub k: usize,
    /// 구독자 비율(0~1).
    pub density: f64,
    pub topic_aware: bool,
    pub hop: HopLatency,
    /// 새로 만드는 그래프 수와 그래프마다 고르는 작성자 수.
    pub graphs: usize,
    pub authors: usize,
    pub seed: u64,
}

impl GossipSpec {
    pub fn new(nodes: usize, k: usize, density: f64, topic_aware: bool, hop: HopLatency) -> Self {
        GossipSpec {
            nodes,
            k,
            density,
            topic_aware,
            hop,
            graphs: 2,
            authors: 24,
            seed: 7,
        }
    }

    fn subscribers(&self) -> usize {
        ((self.density * self.nodes as f64).round() as usize).clamp(2, self.nodes)
    }
}

/// 한 칸의 결과.
#[derive(Debug, Clone, Serialize)]
pub struct GossipRow {
    pub nodes: usize,
    pub k: usize,
    pub density: f64,
    pub topic_aware: bool,
    pub subscribers: usize,
    /// 구독자 하나가 가진 구독 이웃 수 평균.
    pub sub_peers: f64,
    /// 메시가 생긴(구독 이웃 ≥ mesh_n_low) 구독자 비율.
    pub p_mesh: f64,
    /// 메시 간선만으로 이어진 가장 큰 덩어리가 구독자 중 차지하는 비율.
    pub mesh_component: f64,
    /// 발행 한 번에 메시·flood로 닿은 구독자 비율(작성자 빼고, 직접 보내기 대체는 빼고).
    pub mesh_reach: f64,
    /// IHAVE 소문과 직접 보내기 대체까지 넣어 닿은 비율.
    pub delivered: f64,
    /// 모든 구독자 기준 분위(못 닿은 구독자는 무한대로 셈). 그 분위까지 못 닿으면 `None`.
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    /// 메시·flood 간선 홉 수 분포(1홉부터, 구독자 비율). 소문으로만 받은 비율과 못 받은 비율은 따로.
    pub hops: Vec<f64>,
    pub lazy_only: f64,
    /// `NoPeers` 직접 보내기로 받은 비율.
    pub direct_only: f64,
    pub unreached: f64,
    /// 작성자에게 구독 이웃이 없어 `NoPeers`로 직접 보내기에 기댄 비율.
    pub no_peers: f64,
    /// 작성자가 올리는 사본 수 평균(flood_publish 켬 / 끔).
    pub upload_flood: f64,
    pub upload_no_flood: f64,
    pub elapsed_ms: u64,
}

/// 무작위 짝짓기로 만든 단순 그래프(차수 ≈ k).
fn build_graph(spec: &GossipSpec, subs: &[bool], rng: &mut StdRng) -> Vec<Vec<u32>> {
    let n = spec.nodes;
    let sub_count = subs.iter().filter(|s| **s).count();
    let half = if spec.topic_aware {
        (spec.k / 2).min(sub_count.saturating_sub(1))
    } else {
        0
    };
    let mut adj: Vec<Vec<u32>> = vec![Vec::with_capacity(spec.k); n];
    let mut sub_stubs = Vec::with_capacity(sub_count * half);
    let mut all_stubs = Vec::with_capacity(n * spec.k);
    for (v, s) in subs.iter().enumerate() {
        let reserved = if *s { half } else { 0 };
        sub_stubs.extend(std::iter::repeat_n(v as u32, reserved));
        all_stubs.extend(std::iter::repeat_n(v as u32, spec.k - reserved));
    }
    for stubs in [&mut sub_stubs, &mut all_stubs] {
        stubs.shuffle(rng);
        for c in stubs.as_chunks::<2>().0 {
            if c[0] != c[1] {
                adj[c[0] as usize].push(c[1]);
                adj[c[1] as usize].push(c[0]);
            }
        }
    }
    for l in &mut adj {
        l.sort_unstable();
        l.dedup();
    }
    adj
}

/// 구독자 사이 그래프와 메시.
struct Topic {
    /// 구독자 번호(0..S) → 구독 이웃(구독자 번호).
    peers: Vec<Vec<u32>>,
    mesh: Vec<Vec<u32>>,
}

fn build_topic(adj: &[Vec<u32>], subs: &[bool], rng: &mut StdRng) -> Topic {
    let mut index = vec![u32::MAX; subs.len()];
    let mut s = 0u32;
    for (v, on) in subs.iter().enumerate() {
        if *on {
            index[v] = s;
            s += 1;
        }
    }
    let mut peers: Vec<Vec<u32>> = Vec::with_capacity(s as usize);
    for (v, on) in subs.iter().enumerate() {
        if *on {
            peers.push(
                adj[v]
                    .iter()
                    .filter(|w| subs[**w as usize])
                    .map(|w| index[*w as usize])
                    .collect(),
            );
        }
    }
    // 하트비트는 메시가 `mesh_n_low`보다 작으면 있는 구독 이웃을 `mesh_n`까지 모두 붙입니다(이웃이 그보다 적어도).
    // 전에는 이웃이 `mesh_n_low` 미만인 노드를 메시에서 뺐는데, 12단계 루프백 측정(12노드, 이웃 5개 안팎)과 견주니
    // 소문(IHAVE)으로만 받는 비율을 크게 부풀렸습니다(모형 p95 450~620ms, 측정 41ms).
    let healthy: Vec<bool> = peers.iter().map(|p| !p.is_empty()).collect();
    let mut mesh: Vec<Vec<u32>> = vec![Vec::new(); peers.len()];
    let mut order: Vec<u32> = (0..s).filter(|u| healthy[*u as usize]).collect();
    order.shuffle(rng);
    let mut cands = Vec::new();
    for u in order {
        let u = u as usize;
        cands.clear();
        cands.extend(
            peers[u]
                .iter()
                .copied()
                .filter(|v| healthy[*v as usize] && !mesh[u].contains(v)),
        );
        cands.shuffle(rng);
        for &v in &cands {
            if mesh[u].len() >= MESH_N {
                break;
            }
            // 받는 쪽은 mesh_n_high를 넘으면 다음 하트비트에 잘라 냅니다.
            if mesh[v as usize].len() >= MESH_N_HIGH {
                continue;
            }
            mesh[u].push(v);
            mesh[v as usize].push(u as u32);
        }
    }
    Topic { peers, mesh }
}

/// 메시 간선만으로 이어진 가장 큰 덩어리의 크기.
fn largest_mesh_component(t: &Topic) -> usize {
    let n = t.mesh.len();
    let mut seen = vec![false; n];
    let mut best = 0;
    let mut queue = VecDeque::new();
    for s in 0..n {
        if seen[s] || t.mesh[s].is_empty() {
            continue;
        }
        seen[s] = true;
        queue.push_back(s);
        let mut size = 0;
        while let Some(u) = queue.pop_front() {
            size += 1;
            for &v in &t.mesh[u] {
                if !seen[v as usize] {
                    seen[v as usize] = true;
                    queue.push_back(v as usize);
                }
            }
        }
        best = best.max(size);
    }
    best
}

/// 한 번 발행의 결과를 모읍니다.
#[derive(Default)]
struct Acc {
    times: Vec<f64>,
    hops: Vec<u64>,
    lazy_only: u64,
    direct_only: u64,
    unreached: u64,
    mesh_reached: u64,
    targets: u64,
    no_peers: u64,
    upload_flood: u64,
    upload_no_flood: u64,
    publishes: u64,
}

/// 시간 순서용 키(마이크로초).
fn key(ms: f64) -> u64 {
    (ms * 1_000.0) as u64
}

fn publish(t: &Topic, author: usize, hop: &HopLatency, rng: &mut StdRng, acc: &mut Acc) {
    let n = t.peers.len();
    acc.publishes += 1;
    acc.targets += (n - 1) as u64;
    let neighbours = t.peers[author].len();
    if neighbours == 0 {
        // NoPeers → 모든 기기에 직접 보내기.
        acc.no_peers += 1;
        acc.upload_flood += (n - 1) as u64;
        acc.upload_no_flood += (n - 1) as u64;
        for _ in 1..n {
            acc.times.push(hop.sample(rng));
        }
        acc.direct_only += (n - 1) as u64;
        return;
    }
    acc.upload_flood += neighbours as u64;
    // flood가 꺼져 있으면 메시 피어에게, 메시가 없으면 fanout(구독 이웃 mesh_n개)에게 보냅니다.
    acc.upload_no_flood += if t.mesh[author].is_empty() {
        neighbours.min(MESH_N)
    } else {
        t.mesh[author].len()
    } as u64;

    // 홉 수: 작성자 → 구독 이웃(flood) → 메시 간선.
    let mut hops = vec![u32::MAX; n];
    hops[author] = 0;
    let mut queue = VecDeque::new();
    for &v in &t.peers[author] {
        hops[v as usize] = 1;
        queue.push_back(v as usize);
    }
    while let Some(u) = queue.pop_front() {
        for &v in &t.mesh[u] {
            if hops[v as usize] == u32::MAX {
                hops[v as usize] = hops[u] + 1;
                queue.push_back(v as usize);
            }
        }
    }

    // 도착 시각: 같은 간선들 + IHAVE 소문.
    let mut at = vec![f64::INFINITY; n];
    let mut done = vec![false; n];
    let phase: Vec<f64> = (0..n).map(|_| rng.r#gen::<f64>() * HEARTBEAT_MS).collect();
    let mut heap = BinaryHeap::new();
    at[author] = 0.0;
    heap.push(Reverse((0u64, author as u32)));
    let mut lazy = Vec::new();
    while let Some(Reverse((_, u))) = heap.pop() {
        let u = u as usize;
        if done[u] {
            continue;
        }
        done[u] = true;
        let now = at[u];
        let forward: &[u32] = if u == author { &t.peers[u] } else { &t.mesh[u] };
        for &v in forward {
            let v = v as usize;
            if done[v] {
                continue;
            }
            let arrive = now + hop.sample(rng);
            if arrive < at[v] {
                at[v] = arrive;
                heap.push(Reverse((key(arrive), v as u32)));
            }
        }
        // 메시 밖 구독 이웃에게는 다음 하트비트부터 IHAVE를 돌립니다.
        lazy.clear();
        lazy.extend(
            t.peers[u]
                .iter()
                .copied()
                .filter(|v| !t.mesh[u].contains(v) && !done[*v as usize]),
        );
        if u == author || lazy.is_empty() {
            continue;
        }
        let pick = (GOSSIP_LAZY as f64 / lazy.len() as f64).min(1.0);
        let first = now + (phase[u] - now).rem_euclid(HEARTBEAT_MS);
        for &v in &lazy {
            let v = v as usize;
            let Some(beat) = (0..HISTORY_GOSSIP).find(|_| rng.r#gen::<f64>() < pick) else {
                continue;
            };
            // IHAVE → IWANT → 메시지.
            let arrive = first
                + beat as f64 * HEARTBEAT_MS
                + hop.sample(rng)
                + hop.sample(rng)
                + hop.sample(rng);
            if arrive < at[v] {
                at[v] = arrive;
                heap.push(Reverse((key(arrive), v as u32)));
            }
        }
    }
    for v in (0..n).filter(|v| *v != author) {
        acc.times.push(at[v]);
        if hops[v] != u32::MAX {
            acc.mesh_reached += 1;
            let h = hops[v] as usize;
            if acc.hops.len() <= h {
                acc.hops.resize(h + 1, 0);
            }
            acc.hops[h] += 1;
        } else if at[v].is_finite() {
            acc.lazy_only += 1;
        } else {
            acc.unreached += 1;
        }
    }
}

fn percentile(sorted: &[f64], q: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let i = ((sorted.len() - 1) as f64 * q).round() as usize;
    let v = sorted[i.min(sorted.len() - 1)];
    v.is_finite().then_some(v)
}

/// 한 칸을 돌립니다.
pub fn gossip(spec: &GossipSpec) -> GossipRow {
    let started = Instant::now();
    let mut rng = StdRng::seed_from_u64(
        spec.seed ^ (spec.nodes as u64) << 20 ^ (spec.k as u64) << 8 ^ spec.density.to_bits(),
    );
    let s = spec.subscribers();
    let mut acc = Acc::default();
    let (mut healthy, mut peer_sum, mut component) = (0usize, 0usize, 0usize);
    for _ in 0..spec.graphs.max(1) {
        let mut subs = vec![false; spec.nodes];
        for v in rand::seq::index::sample(&mut rng, spec.nodes, s) {
            subs[v] = true;
        }
        let adj = build_graph(spec, &subs, &mut rng);
        let topic = build_topic(&adj, &subs, &mut rng);
        drop(adj);
        healthy += topic.peers.iter().filter(|p| p.len() >= MESH_N_LOW).count();
        peer_sum += topic.peers.iter().map(Vec::len).sum::<usize>();
        component += largest_mesh_component(&topic);
        for _ in 0..spec.authors.max(1) {
            let author = rng.gen_range(0..s);
            publish(&topic, author, &spec.hop, &mut rng, &mut acc);
        }
    }
    let graphs = spec.graphs.max(1) as f64;
    let subs_total = s as f64 * graphs;
    let targets = acc.targets.max(1) as f64;
    acc.times.sort_unstable_by(f64::total_cmp);
    let delivered = acc.times.iter().filter(|t| t.is_finite()).count() as f64;
    let publishes = acc.publishes.max(1) as f64;
    GossipRow {
        nodes: spec.nodes,
        k: spec.k,
        density: spec.density,
        topic_aware: spec.topic_aware,
        subscribers: s,
        sub_peers: peer_sum as f64 / subs_total,
        p_mesh: healthy as f64 / subs_total,
        mesh_component: component as f64 / subs_total,
        mesh_reach: acc.mesh_reached as f64 / targets,
        delivered: delivered / targets,
        p50_ms: percentile(&acc.times, 0.50),
        p95_ms: percentile(&acc.times, 0.95),
        hops: acc
            .hops
            .iter()
            .skip(1)
            .map(|h| *h as f64 / targets)
            .collect(),
        lazy_only: acc.lazy_only as f64 / targets,
        direct_only: acc.direct_only as f64 / targets,
        unreached: acc.unreached as f64 / targets,
        no_peers: acc.no_peers as f64 / publishes,
        upload_flood: acc.upload_flood as f64 / publishes,
        upload_no_flood: acc.upload_no_flood as f64 / publishes,
        elapsed_ms: started.elapsed().as_millis() as u64,
    }
}

/// `nodes` × `ks` × `densities` × (무작위, 토픽 인지) 표를 스레드 여럿으로 돌립니다.
pub fn table(nodes: usize, ks: &[usize], densities: &[f64], hop: HopLatency) -> Vec<GossipRow> {
    let mut specs = Vec::new();
    for &topic_aware in &[false, true] {
        for &k in ks {
            for &f in densities {
                specs.push(GossipSpec::new(nodes, k, f, topic_aware, hop));
            }
        }
    }
    super::state::generate_parallel(specs.len(), |i| Ok(gossip(&specs[i])))
        .expect("gossip 모형은 실패하지 않음")
}

/// 기본 구독자 밀도(작은 비공개 그룹 ~ 크루 전체).
pub const DENSITIES: [f64; 4] = [0.004, 0.01, 0.05, 1.0];

#[cfg(test)]
mod tests {
    use super::*;

    fn row(nodes: usize, k: usize, f: f64, topic_aware: bool) -> GossipRow {
        let mut spec = GossipSpec::new(nodes, k, f, topic_aware, HopLatency::TOR);
        spec.authors = 16;
        gossip(&spec)
    }

    /// 작은 그래프에서 숫자가 말이 되는지: 모두 구독하면 메시가 서고 빠짐없이 닿으며, 드문 토픽은 무작위
    /// 연결로는 메시가 없고 토픽 인지 연결이면 섭니다.
    #[test]
    fn gossip_model_small() {
        let full = row(600, 16, 1.0, false);
        assert!(full.p_mesh > 0.99, "{full:?}");
        assert!(full.delivered > 0.999, "{full:?}");
        assert!(full.no_peers == 0.0, "{full:?}");
        assert!(full.p50_ms.is_some() && full.p95_ms.is_some(), "{full:?}");
        assert!(full.p50_ms <= full.p95_ms);
        assert!(full.upload_flood > full.upload_no_flood, "{full:?}");
        assert!(full.upload_no_flood <= MESH_N_HIGH as f64, "{full:?}");
        let hop_total: f64 =
            full.hops.iter().sum::<f64>() + full.lazy_only + full.direct_only + full.unreached;
        assert!((hop_total - 1.0).abs() < 1e-9, "{full:?}");

        let sparse = row(600, 16, 0.01, false);
        assert_eq!(sparse.subscribers, 6);
        assert!(sparse.p_mesh < 0.2, "{sparse:?}");
        assert!(sparse.no_peers > 0.5, "{sparse:?}");
        assert!(sparse.direct_only > 0.5, "{sparse:?}");
        let total: f64 = sparse.hops.iter().sum::<f64>()
            + sparse.lazy_only
            + sparse.direct_only
            + sparse.unreached;
        assert!((total - 1.0).abs() < 1e-9, "{sparse:?}");

        let aware = row(600, 16, 0.05, true);
        assert!(aware.p_mesh > 0.95, "{aware:?}");
        assert!(aware.delivered > 0.99, "{aware:?}");

        // 같은 설정이면 같은 결과.
        let again = row(600, 16, 0.01, false);
        assert_eq!(
            format!("{sparse:?}").split("elapsed").next(),
            format!("{again:?}").split("elapsed").next()
        );
    }
}
