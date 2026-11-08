//! 키 트리 시험.
//!
//! 무작위 작업열(가입·제거·묶음·시간 회전·R 회전·leaf_master 보유 기기 제거·크루에 남는 보유 기기를 비공개/덮개에서
//! 빼기·비공개/덮개 멤버 바꾸기)에서 다음을 봅니다.
//!
//! - I1: 모든 현재 멤버(크루·비공개·덮개)가 확정 기기와 같은 T·R을 계산하고, 빠진 기기는 적용하지 못합니다.
//! - I2: 그 순번에 멤버가 아닌 기기들이 함께 가진 것(멤버였던 모든 순번의 상태, 넘겨받은 확정 기기 상태, 모든 공개
//!   페이로드)으로 그 순번의 T를 끌어낼 수 없습니다. 같은 값이 다른 순번에 있어도 봐주지 않습니다. 연합에는 이미
//!   빠진 기기와 나중에 들어올 기기가 함께 들어가므로 제거 뒤 비밀과 가입 전 비밀을 한 번에 봅니다.
//! - I3: R은 "R 구간"(새 R을 낸 갱신부터 다음 새 R 전까지) 단위로 판정합니다. 연합 기기 하나라도 멤버였던 구간의 R만
//!   허용합니다(R은 기록 공개 키라 가입자가 그 구간의 R을 받는 것은 설계상 허용).
//! - I4: 조각마다 인코딩이 [`MAX_PAYLOAD_LEN`] 이하이고, 디코딩해 합치면 원래 페이로드입니다. 봉인은 멤버에게만 갑니다.
//!
//! 공격자는 실제 암호로 계산합니다: 알게 된 노드 키로 그 부모 자리의 감싸기 표지를 계산해 열고, 공개 배치로 가입 전진을
//! 따라가고, leaf_master에서 잎을, 크루 잎에서 비공개 잎을, 알게 된 T로 `r_ct`와 직접 인코딩 전진을 계산합니다.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use proptest::prelude::*;
use yangpa_core::body::Secret32;
use yangpa_core::device::BoxPublicKey;
use yangpa_core::ids::{ChannelId, DeviceId, SpaceId};
use yangpa_core::keyupdate::{
    CrewRef, KeyGroupId, KeyUpdateKind, KeyUpdatePayload, LayoutOp, MAX_PAYLOAD_LEN, NodeIdx,
    PART_TARGET_LEN, SealedBox,
};
use yangpa_core::sign::SigningKeypair;

use super::committer::Enc;
use super::*;
use crate::keysched;
use crate::sealed::{BoxKeypair, open_sealed, seal_to, sealed_recipients};

fn dev(i: u32) -> DeviceId {
    let mut b = [0u8; 32];
    b[..4].copy_from_slice(&i.to_be_bytes());
    b[4] = 0xde;
    DeviceId(b)
}

/// 넘기기에 서명하는 확정 기기 키와 그 기기 ID.
fn committer_key() -> (SigningKeypair, DeviceId) {
    let k = SigningKeypair::generate();
    let id = DeviceId(k.public());
    (k, id)
}

fn idx(d: &DeviceId) -> u32 {
    u32::from_be_bytes(d.0[..4].try_into().expect("4바이트"))
}

/// 기기 상자 키. X-Wing 키 생성을 아끼려고 키 쌍 몇 개를 기기들이 나눠 씁니다(봉인은 기기 ID로 나뉨).
struct Env {
    space: SpaceId,
    pool: Vec<BoxKeypair>,
    boxes: BTreeMap<DeviceId, BoxPublicKey>,
}

impl Env {
    fn new() -> Self {
        Env {
            space: SpaceId([7; 16]),
            pool: (0..4).map(|_| BoxKeypair::generate()).collect(),
            boxes: BTreeMap::new(),
        }
    }

    fn add(&mut self, d: DeviceId) {
        let pk = self.kp(&d).public();
        self.boxes.insert(d, pk);
    }

    fn kp(&self, d: &DeviceId) -> &BoxKeypair {
        &self.pool[idx(d) as usize % self.pool.len()]
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn pick(&mut self, from: &[DeviceId], k: usize) -> Vec<DeviceId> {
        let mut v = from.to_vec();
        let k = k.min(v.len());
        for i in 0..k {
            let j = i + (self.next() as usize % (v.len() - i));
            v.swap(i, j);
        }
        v.truncate(k);
        v
    }
}

/// 한 갱신 뒤의 참값(확정 기기 쪽).
struct Truth {
    t: Secret32,
    r: Secret32,
    members: BTreeSet<DeviceId>,
    slots: Vec<(DeviceId, u32)>,
    generation: u64,
}

fn truth_tree(k: &KeyTree) -> Truth {
    Truth {
        t: k.t().clone(),
        r: k.r().clone(),
        members: k.devices().into_iter().collect(),
        slots: k
            .tree()
            .map(|t| t.devices().map(|(s, d)| (*d, s)).collect())
            .unwrap_or_default(),
        generation: k.generation(),
    }
}

fn truth_cover(c: &CoverGroup) -> Truth {
    Truth {
        t: c.t().clone(),
        r: c.r().clone(),
        members: c.members().clone(),
        slots: Vec::new(),
        generation: 0,
    }
}

/// 공개 배치로 정해지는 노드의 운명(공격자도 계산할 수 있음).
struct Rec {
    p: KeyUpdatePayload,
    sites: Vec<WrapSite>,
    t: Secret32,
    r: Secret32,
    members: BTreeSet<DeviceId>,
    slots: Vec<(DeviceId, u32)>,
    generation: u64,
    all_dead: bool,
    dead: HashSet<NodeIdx>,
    adv: HashSet<NodeIdx>,
}

fn fates(p: &KeyUpdatePayload) -> (bool, HashSet<NodeIdx>, HashSet<NodeIdx>) {
    let (mut dead, mut adv) = (HashSet::new(), HashSet::new());
    if p.depth == 0 || matches!(p.kind, KeyUpdateKind::FullRekey { .. }) {
        return (true, dead, adv);
    }
    match p.kind {
        KeyUpdateKind::Refresh => {
            dead.insert(NodeIdx::root(p.depth));
        }
        KeyUpdateKind::Advance { .. } | KeyUpdateKind::Rotate { .. } => {
            for op in &p.layout {
                if let LayoutOp::Clear { slot } = op {
                    dead.extend((0..=p.depth).map(|l| NodeIdx::ancestor(*slot, l)));
                }
            }
            for op in &p.layout {
                if let LayoutOp::Fill { slot, .. } = op {
                    adv.extend(
                        (1..=p.depth)
                            .map(|l| NodeIdx::ancestor(*slot, l))
                            .filter(|n| !dead.contains(n)),
                    );
                }
            }
        }
        _ => {}
    }
    (false, dead, adv)
}

struct GroupSim {
    kind: GroupKind,
    group: KeyGroupId,
    members: BTreeMap<DeviceId, MemberKeys>,
    recs: Vec<Rec>,
    /// 기기마다 거쳐 간 상태(공격자 씨앗).
    states: Vec<(DeviceId, MemberKeys)>,
}

impl GroupSim {
    fn new(kind: GroupKind, group: KeyGroupId) -> Self {
        GroupSim {
            kind,
            group,
            members: BTreeMap::new(),
            recs: Vec::new(),
            states: Vec::new(),
        }
    }

    /// 갱신을 나누고·디코딩하고·합쳐(I4) 모든 기기에 적용하고 확정 기기 참값과 맞춰 봅니다(I1).
    fn deliver(
        &mut self,
        env: &Env,
        out: KeyUpdateOutput,
        sites: Vec<WrapSite>,
        truth: Truth,
        crew: Option<&BTreeMap<DeviceId, MemberKeys>>,
    ) {
        let parts = out.payload.clone().split(PART_TARGET_LEN).expect("나누기");
        let mut decoded = Vec::new();
        for part in &parts {
            let b = part.encode();
            assert!(b.len() <= MAX_PAYLOAD_LEN, "I4: 조각 {}B", b.len());
            decoded.push(KeyUpdatePayload::decode(&b).expect("조각 디코딩"));
        }
        let p = KeyUpdatePayload::merge(decoded).expect("조각 합치기");
        assert_eq!(p, out.payload, "I4: 합친 페이로드가 원래와 다름");
        assert_eq!(sites.len(), p.wraps.len());
        for b in &p.sealed {
            for d in sealed_recipients(&b.0).expect("봉투") {
                assert!(truth.members.contains(&d), "멤버 아닌 기기에게 봉인");
            }
        }
        for b in &out.bundles {
            assert!(truth.members.contains(&b.device), "멤버 아닌 기기에게 묶음");
        }
        // 해소기는 목록만으로 멤버를 따라갑니다(기기 몫이 페이로드 밖에 있는 전체 재키도 목록을 실음): 첫 덮개
        // 갱신(멤버 = 크루 − 제외라 목록이 없는 `Refresh`)이 아니면 멤버 = 앞 멤버 − 제거 + 가입.
        if !(self.kind == GroupKind::Cover && self.members.is_empty()) {
            let mut expect: BTreeSet<DeviceId> = self.members.keys().copied().collect();
            for d in p.kind.removed() {
                assert!(
                    expect.remove(d),
                    "멤버 아닌 기기를 제거 목록에: {:?}",
                    p.kind
                );
            }
            for d in p.kind.joined() {
                assert!(
                    expect.insert(*d),
                    "이미 멤버인 기기를 가입 목록에: {:?}",
                    p.kind
                );
            }
            assert_eq!(expect, truth.members, "목록과 멤버가 다름: {:?}", p.kind);
        }
        let bundles: HashMap<DeviceId, &DeviceBundle> =
            out.bundles.iter().map(|b| (b.device, b)).collect();
        let cx_for = |d: &DeviceId| MemberCtx {
            box_keys: Some(env.kp(d)),
            bundle: bundles.get(d).copied(),
            crew: crew.and_then(|c| c.get(d)),
        };
        let mut next = BTreeMap::new();
        for (d, mk) in &self.members {
            match mk.apply(&p, &cx_for(d)) {
                Ok(n) => {
                    assert!(
                        truth.members.contains(d),
                        "빠진 기기가 적용함: {:?}",
                        p.kind
                    );
                    next.insert(*d, n);
                }
                Err(e) => assert!(
                    !truth.members.contains(d),
                    "I1: 멤버가 적용 실패({:?}, seq {}): {e}",
                    self.kind,
                    p.seq
                ),
            }
        }
        for d in &truth.members {
            if !next.contains_key(d) {
                let n = MemberKeys::join(env.space, self.group, self.kind, *d, &p, &cx_for(d))
                    .unwrap_or_else(|e| {
                        panic!("I1: 가입 실패({:?}, seq {}): {e}", self.kind, p.seq)
                    });
                next.insert(*d, n);
            }
        }
        for n in next.values() {
            assert_eq!(n.t(), &truth.t, "I1: T가 다름");
            assert_eq!(n.r(), &truth.r, "I1: R이 다름");
            assert_eq!(n.seq(), p.seq);
        }
        self.states
            .extend(next.iter().map(|(d, n)| (*d, n.clone())));
        self.members = next;
        let (all_dead, dead, adv) = fates(&p);
        self.recs.push(Rec {
            p,
            sites,
            t: truth.t,
            r: truth.r,
            members: truth.members,
            slots: truth.slots,
            generation: truth.generation,
            all_dead,
            dead,
            adv,
        });
    }
}

/// leaf_master를 가졌던(확정 기기였던) 기기가 아는 것.
struct Holder {
    device: DeviceId,
    /// 그룹마다 이 기기가 확정 기기 상태를 가졌던 마지막 순번.
    until: [u64; 3],
    nodes: Vec<(usize, NodeIdx, Secret32)>,
    leaf_master: Option<(u64, Secret32)>,
    ts: Vec<(usize, Secret32)>,
    rs: Vec<Secret32>,
}

struct Sim {
    env: Env,
    next_id: u32,
    rng: Rng,
    crew: KeyTree,
    private: KeyTree,
    cover: CoverGroup,
    g: [GroupSim; 3],
    private_out: BTreeSet<DeviceId>,
    excluded: BTreeSet<DeviceId>,
    gone: Vec<DeviceId>,
    holders: Vec<Holder>,
}

impl Sim {
    fn new(seed: u64) -> Self {
        let env = Env::new();
        let space = env.space;
        let crew = KeyTree::crew(space);
        let private = KeyTree::private(space, KeyGroupId::private(&space, b"private"));
        let cover = CoverGroup::new(space, KeyGroupId::private(&space, b"cover"));
        let g = [
            GroupSim::new(GroupKind::Crew, *crew.group()),
            GroupSim::new(GroupKind::Private, *private.group()),
            GroupSim::new(GroupKind::Cover, *cover.group()),
        ];
        Sim {
            env,
            next_id: 1,
            rng: Rng(seed | 1),
            crew,
            private,
            cover,
            g,
            private_out: BTreeSet::new(),
            excluded: BTreeSet::new(),
            gone: Vec::new(),
            holders: Vec::new(),
        }
    }

    fn fresh(&mut self, k: usize) -> Vec<DeviceId> {
        (0..k)
            .map(|_| {
                let d = dev(self.next_id);
                self.next_id += 1;
                self.env.add(d);
                if idx(&d).is_multiple_of(7) {
                    self.excluded.insert(d);
                }
                d
            })
            .collect()
    }

    fn crew_devices(&self) -> Vec<DeviceId> {
        self.crew.devices()
    }

    fn after_crew(&mut self, out: KeyUpdateOutput, full: bool) {
        let sites = std::mem::take(&mut self.crew.last_sites);
        let truth = truth_tree(&self.crew);
        self.g[0].deliver(&self.env, out, sites, truth, None);
        self.sync_private(full);
        self.sync_cover(full);
    }

    fn sync_private(&mut self, full: bool) {
        let target: BTreeSet<DeviceId> = self
            .crew_devices()
            .into_iter()
            .filter(|d| idx(d) % 3 != 1 && !self.private_out.contains(d))
            .collect();
        let cur: BTreeSet<DeviceId> = self.private.devices().into_iter().collect();
        let removed: Vec<DeviceId> = cur.difference(&target).copied().collect();
        let joined: Vec<DeviceId> = target.difference(&cur).copied().collect();
        let cx = CommitCtx {
            boxes: &self.env.boxes,
            crew: Some(&self.crew),
        };
        let out = if full {
            self.private.full_rekey(&removed, &joined, &cx)
        } else {
            self.private.update(&removed, &joined, &cx)
        };
        let out = match out {
            Err(KeyUpdateError::NothingToDo) => return,
            r => r.expect("비공개 갱신"),
        };
        let sites = std::mem::take(&mut self.private.last_sites);
        let truth = truth_tree(&self.private);
        let [crew, private, _] = &mut self.g;
        private.deliver(&self.env, out, sites, truth, Some(&crew.members));
    }

    fn sync_cover(&mut self, full: bool) {
        let cx = CommitCtx {
            boxes: &self.env.boxes,
            crew: Some(&self.crew),
        };
        let out = if full {
            Some(self.cover.refresh(&self.excluded, &cx).expect("덮개 회전"))
        } else {
            self.cover.update(&self.excluded, &cx).expect("덮개 갱신")
        };
        let Some(out) = out else { return };
        let sites = std::mem::take(&mut self.cover.last_sites);
        let truth = truth_cover(&self.cover);
        let [crew, _, cover] = &mut self.g;
        cover.deliver(&self.env, out, sites, truth, Some(&crew.members));
    }

    fn crew_update(&mut self, removed: &[DeviceId], joined: &[DeviceId]) {
        let cx = CommitCtx {
            boxes: &self.env.boxes,
            crew: None,
        };
        let out = self.crew.update(removed, joined, &cx).expect("크루 갱신");
        self.gone.extend_from_slice(removed);
        self.after_crew(out, false);
    }

    fn join(&mut self, k: usize) {
        // 4분의 1 정도는 전에 빠졌던 기기가 다시 들어옵니다(같은 세대면 잎이 같음).
        let back: Vec<DeviceId> = {
            let cand: Vec<DeviceId> = self
                .gone
                .iter()
                .filter(|d| !self.crew.contains(d))
                .copied()
                .collect();
            self.rng.pick(&cand, k / 4)
        };
        self.gone.retain(|d| !back.contains(d));
        let mut joined = self.fresh(k - back.len());
        joined.extend(back);
        self.crew_update(&[], &joined);
    }

    fn remove(&mut self, k: usize) {
        let devs = self.crew_devices();
        let k = k.min(devs.len().saturating_sub(1));
        if k == 0 {
            return;
        }
        let removed = self.rng.pick(&devs, k);
        self.crew_update(&removed, &[]);
    }

    fn swap(&mut self, r: usize, j: usize) {
        let devs = self.crew_devices();
        let removed = self.rng.pick(&devs, r.min(devs.len().saturating_sub(1)));
        let joined = self.fresh(j);
        self.crew_update(&removed, &joined);
    }

    fn refresh(&mut self) {
        if self.crew.is_empty() {
            return;
        }
        let cx = CommitCtx {
            boxes: &self.env.boxes,
            crew: None,
        };
        let out = self.crew.refresh_root(&cx).expect("크루 시간 회전");
        self.after_crew(out, false);
        if !self.private.is_empty() {
            let cx = CommitCtx {
                boxes: &self.env.boxes,
                crew: Some(&self.crew),
            };
            let out = self.private.refresh_root(&cx).expect("비공개 시간 회전");
            let sites = std::mem::take(&mut self.private.last_sites);
            let truth = truth_tree(&self.private);
            let [crew, private, _] = &mut self.g;
            private.deliver(&self.env, out, sites, truth, Some(&crew.members));
        }
        self.sync_cover(true);
    }

    fn rrefresh(&mut self) {
        let out = self.crew.refresh_r(&ctx(&self.env)).expect("R 회전");
        let sites = std::mem::take(&mut self.crew.last_sites);
        let truth = truth_tree(&self.crew);
        self.g[0].deliver(&self.env, out, sites, truth, None);
        let cx = CommitCtx {
            boxes: &self.env.boxes,
            crew: Some(&self.crew),
        };
        let out = self.private.refresh_r(&cx).expect("R 회전");
        let sites = std::mem::take(&mut self.private.last_sites);
        let truth = truth_tree(&self.private);
        let [crew, private, cover] = &mut self.g;
        private.deliver(&self.env, out, sites, truth, Some(&crew.members));
        let out = self.cover.refresh_r(&self.excluded, &cx).expect("R 회전");
        let sites = std::mem::take(&mut self.cover.last_sites);
        let truth = truth_cover(&self.cover);
        cover.deliver(&self.env, out, sites, truth, Some(&crew.members));
    }

    /// 지금 확정 기기 상태 전체를 가진 기기(넘겨받았던 기기)라고 치고 표시합니다.
    fn make_holder(&mut self, device: DeviceId) {
        let mut nodes = Vec::new();
        for (g, k) in [(0usize, &self.crew), (1, &self.private)] {
            if let Some(t) = k.tree() {
                for (l, v) in t.levels.iter().enumerate() {
                    for (p, n) in v.iter().enumerate() {
                        if let Some(n) = n {
                            let node = NodeIdx {
                                level: l as u8,
                                pos: p as u32,
                            };
                            nodes.push((g, node, n.key.clone()));
                        }
                    }
                }
            }
        }
        self.holders.push(Holder {
            device,
            until: [self.crew.seq(), self.private.seq(), self.cover.seq()],
            nodes,
            leaf_master: self
                .crew
                .tree()
                .and_then(|t| t.leaf_master.clone())
                .map(|lm| (self.crew.generation(), lm)),
            ts: vec![
                (0, self.crew.t().clone()),
                (1, self.private.t().clone()),
                (2, self.cover.t().clone()),
            ],
            rs: vec![
                self.crew.r().clone(),
                self.private.r().clone(),
                self.cover.r().clone(),
            ],
        });
        self.crew.add_holders([device]);
    }

    /// leaf_master를 가진 기기를 뺍니다: 크루 전체 재키, 비공개 전체 재키, 덮개 새 T.
    fn holder_out(&mut self) {
        let devs = self.crew_devices();
        if devs.len() < 2 {
            return;
        }
        let h = self.rng.pick(&devs, 1)[0];
        self.make_holder(h);
        let cx = CommitCtx {
            boxes: &self.env.boxes,
            crew: None,
        };
        let out = self.crew.full_rekey(&[h], &[], &cx).expect("전체 재키");
        self.gone.push(h);
        self.after_crew(out, true);
    }

    /// 표를 가진 기기가 크루에는 남고 비공개·덮개 그룹에서만 빠짐: 보통 갱신은 거절되고(보유 기기는 leaf_master로 모든
    /// 크루 잎·노드를 계산함), 크루 전체 재키를 먼저 한 묶음에서 뺍니다.
    fn holder_leaves_groups(&mut self) {
        let cand: Vec<DeviceId> = self
            .crew_devices()
            .into_iter()
            .filter(|d| self.private.contains(d) && self.cover.members().contains(d))
            .collect();
        if cand.is_empty() || self.crew.len() < 2 {
            return;
        }
        let h = self.rng.pick(&cand, 1)[0];
        self.make_holder(h);
        let cx = CommitCtx {
            boxes: &self.env.boxes,
            crew: Some(&self.crew),
        };
        let (p_seq, c_seq) = (self.private.seq(), self.cover.seq());
        // 빼고 나서도 트리면 막힘(직접으로 돌아가면 새 T를 남는 기기에게만 봉인하므로 보유 기기도 못 엶).
        if self.private.is_tree() && self.private.len() - 1 > DIRECT_BACK {
            let r = self.private.update(&[h], &[], &cx);
            assert!(
                matches!(r, Err(KeyUpdateError::HolderNeedsFullRekey(d)) if d == h),
                "비공개 트리에서 보유 기기를 보통 제거로 뺌: {r:?}"
            );
            let r = self.private.full_rekey(&[h], &[], &cx);
            assert!(
                matches!(r, Err(KeyUpdateError::HolderNeedsFullRekey(_))),
                "비공개 전체 재키로도 보유 기기는 못 뺌(크루 잎에서 나옴): {r:?}"
            );
        }
        if self.crew.is_tree() {
            let mut ex = self.excluded.clone();
            ex.insert(h);
            let r = self.cover.update(&ex, &cx);
            assert!(
                matches!(r, Err(KeyUpdateError::HolderNeedsFullRekey(d)) if d == h),
                "크루 트리 위 덮개에서 보유 기기를 뺌: {r:?}"
            );
        }
        assert_eq!(
            (self.private.seq(), self.cover.seq()),
            (p_seq, c_seq),
            "실패한 작업이 상태를 바꿈"
        );
        let out = self
            .crew
            .full_rekey(&[], &[], &ctx(&self.env))
            .expect("크루 전체 재키");
        assert!(self.crew.holders().is_empty());
        self.private_out.insert(h);
        self.excluded.insert(h);
        self.after_crew(out, true);
        assert!(!self.private.contains(&h) && !self.cover.members().contains(&h));
    }

    fn toggle(&mut self, k: usize, cover: bool) {
        let devs = self.crew_devices();
        for d in self.rng.pick(&devs, k) {
            let set = if cover {
                &mut self.excluded
            } else {
                &mut self.private_out
            };
            if !set.remove(&d) {
                set.insert(d);
            }
        }
        if cover {
            self.sync_cover(false);
        } else {
            self.sync_private(false);
        }
    }

    fn run(&mut self, op: &Op) {
        match *op {
            Op::Join(k) => self.join(k),
            Op::Remove(k) => self.remove(k),
            Op::Swap(r, j) => self.swap(r, j),
            Op::Refresh => self.refresh(),
            Op::RRefresh => self.rrefresh(),
            Op::HolderOut => self.holder_out(),
            Op::HolderLeavesGroups => self.holder_leaves_groups(),
            Op::TogglePrivate(k) => self.toggle(k, false),
            Op::ToggleCover(k) => self.toggle(k, true),
        }
    }
}

#[derive(Clone, Debug)]
enum Op {
    Join(usize),
    Remove(usize),
    Swap(usize, usize),
    Refresh,
    RRefresh,
    HolderOut,
    HolderLeavesGroups,
    TogglePrivate(usize),
    ToggleCover(usize),
}

// ---- 공격자 ----

enum Item {
    Node(usize, NodeIdx, Secret32, u64),
    T(Secret32),
    CrewLeaf(DeviceId, u64, Secret32),
    LeafMaster(u64, Secret32),
}

/// 모든 연합이 함께 쓰는 색인.
struct Index {
    /// (그룹, 감싸기 자리 노드) → (순번, 감싸기 번호). 덮개 감싸기는 (2, 크루 노드).
    wraps: HashMap<(usize, NodeIdx), Vec<(u64, usize)>>,
    /// T 바이트 → (그룹, 순번).
    t_of: HashMap<[u8; 32], Vec<(usize, u64)>>,
    /// (그룹, 기기, 세대) → 그 세대에 가졌던 칸들.
    slots: HashMap<(usize, DeviceId, u64), BTreeSet<u32>>,
    /// 크루 세대 → 그 세대에 트리에 있던 기기.
    gen_devices: HashMap<u64, BTreeSet<DeviceId>>,
}

impl Index {
    fn build(sim: &Sim) -> Self {
        let mut ix = Index {
            wraps: HashMap::new(),
            t_of: HashMap::new(),
            slots: HashMap::new(),
            gen_devices: HashMap::new(),
        };
        for (g, gs) in sim.g.iter().enumerate() {
            for (i, rec) in gs.recs.iter().enumerate() {
                let seq = i as u64 + 1;
                ix.t_of.entry(rec.t.0).or_default().push((g, seq));
                for (w, site) in rec.sites.iter().enumerate() {
                    ix.wraps.entry((g, site.node)).or_default().push((seq, w));
                }
                for (d, s) in &rec.slots {
                    ix.slots
                        .entry((g, *d, rec.generation))
                        .or_default()
                        .insert(*s);
                    if g == 0 {
                        ix.gen_devices.entry(rec.generation).or_default().insert(*d);
                    }
                }
            }
        }
        ix
    }
}

struct Adv<'a> {
    sim: &'a Sim,
    ix: &'a Index,
    seen: HashSet<(usize, NodeIdx, [u8; 32])>,
    seen_leaf: HashSet<(DeviceId, u64)>,
    flat: HashSet<[u8; 32]>,
    rs: HashSet<[u8; 32]>,
    t_known: HashSet<(usize, u64)>,
    work: Vec<Item>,
}

impl<'a> Adv<'a> {
    fn wcx(&self, g: usize, seq: u64) -> WrapCx<'a> {
        let gs = &self.sim.g[g];
        WrapCx {
            space: &self.sim.env.space,
            group: &gs.group,
            seq,
            salt: &gs.recs[seq as usize - 1].p.salt,
            crew: gs.recs[seq as usize - 1].p.crew.as_ref(),
        }
    }

    fn rec(&self, g: usize, seq: u64) -> Option<&'a Rec> {
        self.sim.g[g].recs.get((seq as usize).checked_sub(1)?)
    }

    fn see_t(&mut self, key: &Secret32) {
        self.flat.insert(key.0);
        let Some(hits) = self.ix.t_of.get(&key.0) else {
            return;
        };
        for &(g, s) in hits {
            if !self.t_known.insert((g, s)) {
                continue;
            }
            let rec = self.rec(g, s).expect("기록");
            let group = &self.sim.g[g].group;
            if let Some(ct) = &rec.p.r_ct
                && let Ok(r) = keysched::open_r(key, &self.sim.env.space, group, s, &rec.p.salt, ct)
            {
                self.rs.insert(r.0);
            }
            // 직접 인코딩·덮개 그룹의 다음 T가 전진으로 나왔는지는 종류와 상관없이 계산해 봅니다(설계상 가입만이지만,
            // 확정 기기가 제거에도 전진을 쓰는 실수를 잡으려고).
            if let Some(nx) = self.rec(g, s + 1)
                && (nx.p.depth == 0 || g == 2)
            {
                let jh = keysched::joined_hash(nx.p.kind.joined());
                self.work
                    .push(Item::T(keysched::advance(key, group, s + 1, &jh)));
            }
        }
    }

    fn learn_node(&mut self, g: usize, node: NodeIdx, key: Secret32, from: u64) {
        if !self.seen.insert((g, node, key.0)) {
            return;
        }
        self.see_t(&key);
        let group = self.sim.g[g].group;
        // 공개 배치를 따라 앞으로: 그대로면 계속, 전진이면 새 키, 새로 뽑혔으면 끝. 새로 뽑혔어야 할 노드도 전진을
        // 계산해 봅니다(확정 기기가 제거 경로를 전진으로 바꾸는 실수를 잡으려고. 맞지 않으면 아무것도 열지 못함).
        if node.level > 0 {
            let mut s = from + 1;
            while let Some(rec) = self.rec(g, s) {
                if rec.all_dead {
                    break;
                }
                if rec.adv.contains(&node) || rec.dead.contains(&node) {
                    let jh = keysched::joined_hash(rec.p.kind.joined());
                    let k = advance_node(&key, &group, s, node, &jh);
                    self.work.push(Item::Node(g, node, k, s));
                    break;
                }
                s += 1;
            }
        }
        // 이 키가 감싼 부모 키.
        let parent = node.parent();
        if let Some(list) = self.ix.wraps.get(&(g, parent)) {
            for &(seq, w) in list {
                let rec = self.rec(g, seq).expect("기록");
                let site = rec.sites[w];
                if site.kind != WrapKind::Tree {
                    continue;
                }
                let cx = self.wcx(g, seq);
                let wrap = &rec.p.wraps[w];
                if cx.tag(&key, site) == wrap.tag
                    && let Some(k) = cx.open(&key, site, wrap)
                {
                    self.work.push(Item::Node(g, parent, k, seq));
                }
            }
        }
        // 크루 노드 키로 덮개 감싸기.
        if g == 0
            && let Some(list) = self.ix.wraps.get(&(2, node))
        {
            for &(seq, w) in list {
                let rec = self.rec(2, seq).expect("기록");
                let site = rec.sites[w];
                let cx = self.wcx(2, seq);
                let wrap = &rec.p.wraps[w];
                if cx.tag(&key, site) == wrap.tag
                    && let Some(k) = cx.open(&key, site, wrap)
                {
                    self.work.push(Item::T(k));
                }
            }
        }
    }

    fn crew_leaf(&mut self, d: DeviceId, generation: u64, key: Secret32) {
        if !self.seen_leaf.insert((d, generation)) {
            return;
        }
        if let Some(slots) = self.ix.slots.get(&(0, d, generation)) {
            for &s in slots {
                self.work
                    .push(Item::Node(0, NodeIdx::leaf(s), key.clone(), 0));
            }
        }
        if let Some(slots) = self.ix.slots.get(&(1, d, generation)) {
            let pk = private_leaf(&key, &self.sim.g[1].group, generation);
            for &s in slots {
                self.work
                    .push(Item::Node(1, NodeIdx::leaf(s), pk.clone(), 0));
            }
        }
    }

    /// 연합 `c`가 끌어낼 수 있는 것을 모두 계산하고, 허용되지 않은 T·R을 찾습니다.
    fn violations(sim: &Sim, ix: &Index, c: &BTreeSet<DeviceId>) -> Vec<String> {
        let mut a = Adv {
            sim,
            ix,
            seen: HashSet::new(),
            seen_leaf: HashSet::new(),
            flat: HashSet::new(),
            rs: HashSet::new(),
            t_known: HashSet::new(),
            work: Vec::new(),
        };
        for (g, gs) in sim.g.iter().enumerate() {
            for (d, mk) in &gs.states {
                if !c.contains(d) {
                    continue;
                }
                a.work.push(Item::T(mk.t().clone()));
                a.rs.insert(mk.r().0);
                if let Some(tp) = mk.tree_pos() {
                    if g == 0 {
                        a.work
                            .push(Item::CrewLeaf(*d, tp.generation, tp.leaf.key.clone()));
                    } else {
                        a.work.push(Item::Node(
                            g,
                            NodeIdx::leaf(tp.slot),
                            tp.leaf.key.clone(),
                            mk.seq(),
                        ));
                    }
                    for (node, n) in tp.nodes().skip(1) {
                        a.work.push(Item::Node(g, node, n.key.clone(), mk.seq()));
                    }
                }
            }
        }
        for h in sim.holders.iter().filter(|h| c.contains(&h.device)) {
            for (g, node, key) in &h.nodes {
                a.work.push(Item::Node(*g, *node, key.clone(), h.until[*g]));
            }
            if let Some((generation, lm)) = &h.leaf_master {
                a.work.push(Item::LeafMaster(*generation, lm.clone()));
            }
            for (_, t) in &h.ts {
                a.work.push(Item::T(t.clone()));
            }
            a.rs.extend(h.rs.iter().map(|r| r.0));
        }
        while let Some(item) = a.work.pop() {
            match item {
                Item::Node(g, node, key, from) => a.learn_node(g, node, key, from),
                Item::T(t) => a.see_t(&t),
                Item::CrewLeaf(d, generation, key) => a.crew_leaf(d, generation, key),
                Item::LeafMaster(generation, lm) => {
                    let devs: Vec<DeviceId> = ix
                        .gen_devices
                        .get(&generation)
                        .map(|s| s.iter().copied().collect())
                        .unwrap_or_default();
                    for d in devs {
                        let key = leaf_key(&lm, &sim.g[0].group, &d, generation);
                        a.work.push(Item::CrewLeaf(d, generation, key));
                    }
                }
            }
        }
        // 판정: 연합 기기가 하나도 자격이 없던 순번의 T·R.
        let mut out = Vec::new();
        for (g, gs) in sim.g.iter().enumerate() {
            let eligible = |i: usize, rec: &Rec| {
                rec.members.iter().any(|d| c.contains(d))
                    || sim
                        .holders
                        .iter()
                        .any(|h| c.contains(&h.device) && (i as u64) < h.until[g])
            };
            // R 구간: 새 R을 내는 갱신마다 새 구간(가입만이면 같은 구간이라 가입자가 받아도 됨).
            let mut epoch = Vec::with_capacity(gs.recs.len());
            let mut e = 0usize;
            for r in &gs.recs {
                if r.p.kind.new_r() {
                    e += 1;
                }
                epoch.push(e);
            }
            let allowed_epoch: HashSet<usize> = gs
                .recs
                .iter()
                .enumerate()
                .filter(|(i, r)| eligible(*i, r))
                .map(|(i, _)| epoch[i])
                .collect();
            for (i, rec) in gs.recs.iter().enumerate() {
                if eligible(i, rec) {
                    continue;
                }
                // T는 값이 같은 다른 순번이 허용돼도 봐주지 않음(R만 회전은 멤버가 같아 자격도 같음).
                if a.flat.contains(&rec.t.0) {
                    out.push(format!("I2: 그룹 {g} 순번 {}의 T", i + 1));
                }
                if a.rs.contains(&rec.r.0) && !allowed_epoch.contains(&epoch[i]) {
                    out.push(format!("I3: 그룹 {g} 순번 {}의 R", i + 1));
                }
            }
        }
        out
    }
}

/// 기록된 모든 순번의 "그 순번 비멤버" 연합마다 공격자를 돌립니다.
fn adversary_violations(sim: &Sim) -> Vec<String> {
    let ix = Index::build(sim);
    let universe: BTreeSet<DeviceId> = sim.env.boxes.keys().copied().collect();
    let mut coalitions: BTreeSet<BTreeSet<DeviceId>> = BTreeSet::new();
    for gs in &sim.g {
        for rec in &gs.recs {
            coalitions.insert(universe.difference(&rec.members).copied().collect());
        }
    }
    coalitions.remove(&BTreeSet::new());
    let mut out = Vec::new();
    for c in &coalitions {
        out.extend(Adv::violations(sim, &ix, c));
    }
    out
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (1usize..=150).prop_map(Op::Join),
        3 => (1usize..=200).prop_map(Op::Remove),
        2 => (1usize..=40, 1usize..=40).prop_map(|(r, j)| Op::Swap(r, j)),
        1 => Just(Op::Refresh),
        1 => Just(Op::RRefresh),
        1 => Just(Op::HolderOut),
        1 => Just(Op::HolderLeavesGroups),
        1 => (1usize..=40).prop_map(Op::TogglePrivate),
        1 => (1usize..=40).prop_map(Op::ToggleCover),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 10,
        max_shrink_iters: 16,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// 1..600대에서 무작위 작업열: I1·I4는 매 갱신, I2·I3은 끝에 모든 연합으로.
    #[test]
    fn random_ops_keep_invariants(
        n0 in 1usize..=600,
        ops in proptest::collection::vec(op(), 1..=6),
        seed in any::<u64>(),
    ) {
        let mut sim = Sim::new(seed);
        sim.join(n0);
        for o in &ops {
            sim.run(o);
        }
        let v = adversary_violations(&sim);
        prop_assert!(v.is_empty(), "{v:?}");
    }
}

// ---- 정해진 시나리오 ----

#[test]
fn tree_scenario_keeps_secrets_across_modes() {
    let mut sim = Sim::new(11);
    sim.join(40);
    sim.join(260); // 직접 → 트리(전체 재키)
    assert!(sim.crew.is_tree());
    sim.remove(3);
    sim.swap(5, 7);
    sim.join(30);
    sim.refresh();
    sim.rrefresh();
    sim.toggle(10, true);
    sim.toggle(10, false);
    sim.remove(170); // 트리 → 직접
    assert!(!sim.crew.is_tree());
    sim.join(5);
    let v = adversary_violations(&sim);
    assert!(v.is_empty(), "{v:?}");
}

#[test]
fn direct_scenario_keeps_secrets() {
    let mut sim = Sim::new(5);
    sim.join(6);
    sim.join(3);
    sim.remove(2);
    sim.swap(1, 2);
    sim.refresh();
    sim.rrefresh();
    sim.holder_out();
    sim.join(1);
    assert!(!sim.crew.is_tree());
    let v = adversary_violations(&sim);
    assert!(v.is_empty(), "{v:?}");
}

#[test]
fn full_rekey_cuts_old_leaf_master_holders() {
    let mut sim = Sim::new(3);
    sim.join(300);
    sim.join(4);
    sim.holder_out();
    sim.join(2);
    sim.remove(2);
    let v = adversary_violations(&sim);
    assert!(v.is_empty(), "{v:?}");
}

/// 같은 장면에서 leaf_master 보유 기기를 보통 제거로 빼면 공격자가 새 T를 끌어냅니다(공격자 모형이 실제로 일하는지).
#[test]
fn plain_removal_of_a_holder_is_caught() {
    let mut sim = Sim::new(3);
    sim.join(300);
    let h = sim.crew_devices()[17];
    sim.make_holder(h);
    let saved = sim.crew.clone();
    assert!(matches!(
        sim.crew.update(&[h], &[], &ctx(&sim.env)),
        Err(KeyUpdateError::HolderNeedsFullRekey(d)) if d == h
    ));
    assert_eq!(sim.crew.seq(), saved.seq(), "거절된 작업이 상태를 바꿈");
    // 막는 장치를 건너뛰면(보유 기기 목록을 잊음) 공격자 모형이 새 T를 끌어냅니다.
    sim.crew.holders.clear();
    sim.crew_update(&[h], &[]);
    let v = adversary_violations(&sim);
    assert!(
        v.iter().any(|s| s.starts_with("I2: 그룹 0")),
        "보유 기기가 새 T를 끌어내야 함: {v:?}"
    );
}

/// 공격자가 정말 감싸기를 따라가는지: 남은 기기 하나를 연합에 넣으면 지금 T가 보입니다.
#[test]
fn adversary_follows_wraps() {
    let mut sim = Sim::new(9);
    sim.join(300);
    sim.remove(4);
    sim.join(3);
    let ix = Index::build(&sim);
    let member = *sim.g[0].members.keys().next().expect("멤버");
    let c: BTreeSet<DeviceId> = [member].into_iter().collect();
    let mut a = Adv {
        sim: &sim,
        ix: &ix,
        seen: HashSet::new(),
        seen_leaf: HashSet::new(),
        flat: HashSet::new(),
        rs: HashSet::new(),
        t_known: HashSet::new(),
        work: Vec::new(),
    };
    // 첫 상태(전체 재키 직후)만 주고 나머지는 감싸기·전진으로 따라가게 합니다.
    let first = sim.g[0]
        .states
        .iter()
        .find(|(d, _)| c.contains(d))
        .expect("상태")
        .1
        .clone();
    let tp = first.tree_pos().expect("트리");
    a.work
        .push(Item::CrewLeaf(member, tp.generation, tp.leaf.key.clone()));
    for (node, n) in tp.nodes().skip(1) {
        a.work.push(Item::Node(0, node, n.key.clone(), first.seq()));
    }
    while let Some(item) = a.work.pop() {
        match item {
            Item::Node(g, node, key, from) => a.learn_node(g, node, key, from),
            Item::T(t) => a.see_t(&t),
            Item::CrewLeaf(d, generation, key) => a.crew_leaf(d, generation, key),
            Item::LeafMaster(..) => {}
        }
    }
    assert!(a.flat.contains(&sim.crew.t().0));
    assert!(a.rs.contains(&sim.crew.r().0));
}

#[test]
fn joiner_cannot_open_pre_join_private_roots() {
    let mut sim = Sim::new(21);
    sim.join(280);
    let ch = ChannelId([4; 16]);
    let before = keysched::private_root(sim.crew.t(), &sim.env.space, &ch);
    sim.join(2);
    let joiner = *sim.g[0]
        .members
        .keys()
        .find(|d| idx(d) > 280)
        .expect("가입자");
    let mk = &sim.g[0].members[&joiner];
    assert_ne!(mk.private_root(&ch), before);
    assert_eq!(
        mk.private_root(&ch),
        keysched::private_root(sim.crew.t(), &sim.env.space, &ch)
    );
    // R은 그대로라 공개 채널 뿌리는 가입 전 것과 같음(기록 공개).
    assert_eq!(
        mk.public_root(&ch),
        keysched::public_root(sim.crew.r(), &sim.env.space, &ch)
    );
    let v = adversary_violations(&sim);
    assert!(v.is_empty(), "{v:?}");
}

// ---- 단위 시험 ----

fn ctx(env: &Env) -> CommitCtx<'_> {
    CommitCtx {
        boxes: &env.boxes,
        crew: None,
    }
}

#[test]
fn wrap_ad_binds_everything() {
    let (space, group) = (SpaceId([1; 16]), KeyGroupId([2; 16]));
    let salt = [3u8; 24];
    let cx = WrapCx {
        space: &space,
        group: &group,
        seq: 9,
        salt: &salt,
        crew: None,
    };
    let k = Secret32::random();
    let v = Secret32::random();
    let site = WrapSite {
        kind: WrapKind::Tree,
        node: NodeIdx { level: 2, pos: 1 },
        version: 9,
    };
    let w = cx.seal(&k, site, &v);
    assert_eq!(cx.open(&k, site, &w), Some(v.clone()));
    assert!(cx.open(&Secret32::random(), site, &w).is_none());
    let other_node = WrapSite {
        node: NodeIdx { level: 2, pos: 0 },
        ..site
    };
    assert!(cx.open(&k, other_node, &w).is_none());
    let other_version = WrapSite { version: 8, ..site };
    assert!(cx.open(&k, other_version, &w).is_none());
    let cover = WrapSite {
        kind: WrapKind::Cover,
        ..site
    };
    assert!(cx.open(&k, cover, &w).is_none());
    let seq = WrapCx { seq: 10, ..cx };
    assert!(seq.open(&k, site, &w).is_none());
    let g2 = KeyGroupId([5; 16]);
    assert!(WrapCx { group: &g2, ..cx }.open(&k, site, &w).is_none());
    let s2 = SpaceId([6; 16]);
    assert!(WrapCx { space: &s2, ..cx }.open(&k, site, &w).is_none());
    let salt2 = [4u8; 24];
    assert!(WrapCx { salt: &salt2, ..cx }.open(&k, site, &w).is_none());
    // 기대는 크루 상태도 묶임: 있는지 없는지, 순번, 약속.
    let anchor = CrewRef {
        seq: 4,
        root_commit: [8; 32],
    };
    let with = WrapCx {
        crew: Some(&anchor),
        ..cx
    };
    let w2 = with.seal(&k, site, &v);
    assert_eq!(with.open(&k, site, &w2), Some(v.clone()));
    assert!(cx.open(&k, site, &w2).is_none());
    assert!(with.open(&k, site, &w).is_none());
    for other in [
        CrewRef { seq: 5, ..anchor },
        CrewRef {
            root_commit: [9; 32],
            ..anchor
        },
    ] {
        let cx3 = WrapCx {
            crew: Some(&other),
            ..cx
        };
        assert!(cx3.open(&k, site, &w2).is_none());
    }
}

#[test]
fn tags_differ_across_seqs_groups_and_nodes() {
    let (space, group) = (SpaceId([1; 16]), KeyGroupId([2; 16]));
    let salt = [0u8; 24];
    let k = Secret32::random();
    let site = WrapSite {
        kind: WrapKind::Tree,
        node: NodeIdx { level: 1, pos: 0 },
        version: 1,
    };
    let mut tags = HashSet::new();
    for seq in 1..50 {
        let cx = WrapCx {
            space: &space,
            group: &group,
            seq,
            salt: &salt,
            crew: None,
        };
        assert!(tags.insert(cx.tag(&k, site)), "순번마다 표지가 달라야 함");
    }
    let cx = WrapCx {
        space: &space,
        group: &group,
        seq: 1,
        salt: &salt,
        crew: None,
    };
    let g2 = KeyGroupId([9; 16]);
    assert_ne!(cx.tag(&k, site), WrapCx { group: &g2, ..cx }.tag(&k, site));
    let n2 = WrapSite {
        node: NodeIdx { level: 1, pos: 1 },
        ..site
    };
    assert_ne!(cx.tag(&k, site), cx.tag(&k, n2));
    // 같은 갱신 안에서도 감싸는 키가 다르면 표지가 다름(받는 쪽이 자기 감싸기를 찾음).
    assert_ne!(cx.tag(&k, site), cx.tag(&Secret32::random(), site));
}

/// 직접 인코딩 묶음 하나를 손으로 만들어 넣어 확정 기기가 기기마다 다른 키를 주는 경우를 흉내 냅니다.
#[test]
fn root_commit_and_chain_checks() {
    let mut env = Env::new();
    let devs: Vec<DeviceId> = (1..=4).map(dev).collect();
    for d in &devs {
        env.add(*d);
    }
    let mut crew = KeyTree::crew(env.space);
    let out = crew.update(&[], &devs, &ctx(&env)).expect("만들기");
    let cxm = |d: &DeviceId| MemberCtx {
        box_keys: Some(env.kp(d)),
        ..MemberCtx::default()
    };
    let group = *crew.group();
    let a = MemberKeys::join(
        env.space,
        group,
        GroupKind::Crew,
        devs[0],
        &out.payload,
        &cxm(&devs[0]),
    )
    .expect("가입");
    let out2 = crew.update(&[devs[3]], &[], &ctx(&env)).expect("제거");
    let good = a.apply(&out2.payload, &cxm(&devs[0])).expect("적용");
    assert_eq!(good.t(), crew.t());

    // 다른 T를 봉인한 상자: 뿌리 약속 불일치.
    let mut forged = out2.payload.clone();
    let to: Vec<_> = devs[..3]
        .iter()
        .map(|d| (*d, env.boxes[d].clone()))
        .collect();
    let fake = Bundle::Direct {
        t: Secret32::random(),
        r: None,
    };
    forged.sealed = vec![SealedBox(seal_to(
        &to,
        &direct_ad(&env.space, &group, forged.seq, &forged.root_commit),
        &fake.encode(),
    ))];
    assert!(matches!(
        a.apply(&forged, &cxm(&devs[0])),
        Err(KeyUpdateError::RootCommitMismatch)
    ));
    // 약속 자체를 바꾸면 상자 AD가 약속을 묶으므로 상자부터 열리지 않음(다른 갈래의 상자도 마찬가지).
    let mut bad = out2.payload.clone();
    bad.root_commit[0] ^= 1;
    assert!(matches!(
        a.apply(&bad, &cxm(&devs[0])),
        Err(KeyUpdateError::Crypto(_))
    ));

    // 앞 약속이 다르면 다른 갈래.
    let mut fork = out2.payload.clone();
    fork.prev_root_commit[5] ^= 1;
    assert!(matches!(
        a.apply(&fork, &cxm(&devs[0])),
        Err(KeyUpdateError::ChainMismatch)
    ));
    // 순번을 건너뛰면 적용하지 않음.
    let out3 = crew.refresh_r(&ctx(&env)).expect("R 회전");
    assert!(matches!(
        a.apply(&out3.payload, &cxm(&devs[0])),
        Err(KeyUpdateError::SeqGap { have: 1, got: 3 })
    ));
    // 상자가 없는 갱신(R만 회전)은 약속 확인에서 걸림.
    let mut bad = out3.payload.clone();
    bad.root_commit[0] ^= 1;
    assert!(matches!(
        good.apply(&bad, &cxm(&devs[0])),
        Err(KeyUpdateError::RootCommitMismatch)
    ));
    let next = good.apply(&out3.payload, &cxm(&devs[0])).expect("R 회전");
    assert_eq!(next.r(), crew.r());
    assert_eq!(next.t(), good.t());
    // 다른 그룹.
    let mut other = out3.payload.clone();
    other.group = KeyGroupId([1; 16]);
    assert!(matches!(
        good.apply(&other, &cxm(&devs[0])),
        Err(KeyUpdateError::WrongGroup)
    ));
    // 빠진 기기.
    let gone = MemberKeys::join(
        env.space,
        group,
        GroupKind::Crew,
        devs[3],
        &out.payload,
        &cxm(&devs[3]),
    )
    .expect("가입");
    assert!(matches!(
        gone.apply(&out2.payload, &cxm(&devs[3])),
        Err(KeyUpdateError::Removed)
    ));
}

/// 직접 인코딩: 빠진 기기는 새 상자의 수신자가 아니고, 가입자는 전진한 T만 받습니다.
#[test]
fn direct_mode_equivalents() {
    let mut env = Env::new();
    let devs: Vec<DeviceId> = (1..=6).map(dev).collect();
    for d in &devs {
        env.add(*d);
    }
    let mut crew = KeyTree::crew(env.space);
    let first = crew.update(&[], &devs[..4], &ctx(&env)).expect("만들기");
    let t1 = crew.t().clone();
    let out = crew.update(&[], &devs[4..5], &ctx(&env)).expect("가입");
    assert!(out.payload.r_ct.is_none(), "가입만이면 R 그대로");
    let to = sealed_recipients(&out.payload.sealed[0].0).expect("봉투");
    assert_eq!(to, vec![devs[4]], "가입 상자는 가입자에게만");
    let t2 = crew.t().clone();
    assert_eq!(
        t2,
        keysched::advance(&t1, crew.group(), 2, &keysched::joined_hash(&devs[4..5]))
    );
    let r_before = crew.r().clone();
    let out = crew
        .update(&devs[1..2], &devs[5..6], &ctx(&env))
        .expect("제거");
    let to: BTreeSet<DeviceId> = sealed_recipients(&out.payload.sealed[0].0)
        .expect("봉투")
        .into_iter()
        .collect();
    assert!(!to.contains(&devs[1]));
    assert_eq!(to.len(), 5);
    assert_ne!(crew.r(), &r_before);
    assert_ne!(crew.t(), &t2);
    // 순번 1의 상자를 받은 기기들도 순번 3의 T를 그 상자로 열 수는 없음(새 무작위 T).
    assert_eq!(first.payload.sealed.len(), 1);
    assert_eq!(out.payload.depth, 0);
}

#[test]
fn bundle_decode_caps() {
    let ok = Bundle::Tree {
        slot: 0,
        generation: 1,
        leaf: Secret32::random(),
        path: vec![Secret32::random(); 17],
        r: None,
    };
    assert!(Bundle::decode(&ok.encode()).is_ok());
    for len in [0, 18, 1000] {
        let bad = Bundle::Tree {
            slot: 0,
            generation: 1,
            leaf: Secret32::random(),
            path: vec![Secret32::random(); len],
            r: None,
        };
        assert!(Bundle::decode(&bad.encode()).is_err(), "경로 {len}");
    }
    assert!(Bundle::decode(&[0xff; 10]).is_err());
}

fn brute_min_cover(depth: u8, slots: &[Option<DeviceId>], excluded: &BTreeSet<DeviceId>) -> usize {
    let nodes: Vec<NodeIdx> = (0..=depth)
        .flat_map(|l| (0..(1u32 << (depth - l))).map(move |p| NodeIdx { level: l, pos: p }))
        .collect();
    let members: Vec<u32> = slots
        .iter()
        .enumerate()
        .filter(|(_, d)| d.is_some_and(|d| !excluded.contains(&d)))
        .map(|(s, _)| s as u32)
        .collect();
    let excl: Vec<u32> = slots
        .iter()
        .enumerate()
        .filter(|(_, d)| d.is_some_and(|d| excluded.contains(&d)))
        .map(|(s, _)| s as u32)
        .collect();
    let usable: Vec<NodeIdx> = nodes
        .into_iter()
        .filter(|n| !excl.iter().any(|&s| n.covers(s)))
        .collect();
    let mut best = usize::MAX;
    for mask in 0u32..(1 << usable.len()) {
        let chosen: Vec<&NodeIdx> = usable
            .iter()
            .enumerate()
            .filter(|(i, _)| mask >> i & 1 == 1)
            .map(|(_, n)| n)
            .collect();
        if members.iter().all(|&s| chosen.iter().any(|n| n.covers(s))) {
            best = best.min(chosen.len());
        }
    }
    best
}

#[test]
fn exclusion_cover_is_exact_and_minimal() {
    let mut rng = Rng(77);
    for round in 0..300 {
        let depth = 1 + (round % 6) as u8;
        let n = 1usize << depth;
        let slots: Vec<Option<DeviceId>> = (0..n)
            .map(|i| (!rng.next().is_multiple_of(4)).then(|| dev(i as u32 + 1)))
            .collect();
        let excluded: BTreeSet<DeviceId> = slots
            .iter()
            .flatten()
            .filter(|_| rng.next().is_multiple_of(5))
            .copied()
            .collect();
        let cover = exclusion_cover(depth, &slots, &excluded);
        for (s, d) in slots.iter().enumerate() {
            let hits = cover.iter().filter(|c| c.covers(s as u32)).count();
            match d {
                Some(d) if excluded.contains(d) => assert_eq!(hits, 0, "제외 기기를 덮음"),
                Some(_) => assert_eq!(hits, 1, "멤버를 정확히 한 번"),
                None => assert!(hits <= 1),
            }
        }
        for c in &cover {
            assert!(
                slots
                    .iter()
                    .enumerate()
                    .any(|(s, d)| c.covers(s as u32) && d.is_some_and(|d| !excluded.contains(&d)))
            );
        }
        if depth <= 3 {
            assert_eq!(
                cover.len(),
                brute_min_cover(depth, &slots, &excluded),
                "최소"
            );
        }
    }
    // 제외가 없으면 뿌리 하나, 기기가 없으면 비어 있음.
    let slots = vec![Some(dev(1)), None, Some(dev(2)), None];
    assert_eq!(
        exclusion_cover(2, &slots, &BTreeSet::new()),
        vec![NodeIdx::root(2)]
    );
    assert!(exclusion_cover(2, &[None; 4], &BTreeSet::new()).is_empty());
}

#[test]
fn handover_continues_the_chain() {
    let mut sim = Sim::new(31);
    sim.join(290);
    sim.toggle(5, true);
    let to_dev = sim.crew_devices()[0];
    let to = vec![(to_dev, sim.env.boxes[&to_dev].clone())];
    let (signer, from) = committer_key();
    let sealed =
        seal_handover(&[&sim.crew, &sim.private], &[&sim.cover], &to, &signer).expect("봉인");
    let (mut trees, mut covers) =
        open_handover(&to_dev, sim.env.kp(&to_dev), &sealed, &from).expect("열기");
    // 다른 기기는 못 엶.
    let other = sim.crew_devices()[1];
    assert!(open_handover(&other, sim.env.kp(&other), &sealed, &from).is_err());
    let private = trees.pop().expect("비공개");
    let crew = trees.pop().expect("크루");
    assert_eq!(crew.t(), sim.crew.t());
    assert_eq!(crew.devices(), sim.crew.devices());
    assert_eq!(private.t(), sim.private.t());
    // 넘겨준 기기는 표 보유 기기로 남음(받은 나는 빠짐).
    assert!(crew.holders().contains(&from) && !crew.holders().contains(&to_dev));
    assert_eq!(crew.tip(), sim.crew.tip());
    sim.crew = crew;
    sim.private = private;
    sim.cover = covers.pop().expect("덮개");
    // 넘겨받은 상태로 계속 갱신해도 멤버가 따라옴.
    sim.remove(3);
    sim.join(2);
    sim.toggle(3, true);
    assert_eq!(sim.g[0].members.len(), sim.crew.len());
}

#[test]
fn private_and_cover_groups_need_their_inputs() {
    let mut sim = Sim::new(41);
    sim.join(420);
    assert!(sim.private.is_tree(), "비공개 그룹도 트리");
    // 비공개 트리는 크루 트리 없이는 갱신하지 않음.
    let cx = ctx(&sim.env);
    let d = sim.private.devices()[0];
    assert!(matches!(
        sim.private.update(&[d], &[], &cx),
        Err(KeyUpdateError::NeedCrewTree)
    ));
    assert!(matches!(
        sim.cover.update(&BTreeSet::new(), &cx),
        Err(KeyUpdateError::NeedCrewTree)
    ));
    // 덮개 멤버가 크루 키 없이 열려 하면 NeedCrewTree.
    let cx = CommitCtx {
        boxes: &sim.env.boxes,
        crew: Some(&sim.crew),
    };
    let mut excluded = sim.excluded.clone();
    let victim = *sim.cover.members().iter().next().expect("멤버");
    excluded.insert(victim);
    let out = sim
        .cover
        .update(&excluded, &cx)
        .expect("덮개")
        .expect("바뀜");
    let m = sim.g[2]
        .members
        .values()
        .find(|m| m.device() != &victim)
        .expect("멤버");
    assert!(matches!(
        m.apply(&out.payload, &MemberCtx::default()),
        Err(KeyUpdateError::NeedCrewTree)
    ));
    // 덮개는 기기 수가 아니라 제외 기기 수에 비례.
    assert!(out.payload.wraps.len() <= (excluded.len() + 1) * usize::from(sim.crew.depth()));
}

#[test]
fn private_tree_is_rebuilt_after_crew_full_rekey() {
    let mut sim = Sim::new(51);
    sim.join(420);
    assert!(sim.private.is_tree());
    let generation = sim.private.generation;
    sim.holder_out();
    assert_eq!(sim.private.generation, generation + 1);
    assert_eq!(sim.private.generation, sim.crew.generation());
    let cx = CommitCtx {
        boxes: &sim.env.boxes,
        crew: Some(&sim.crew),
    };
    assert!(!sim.private.needs_rebuild(&sim.crew));
    assert!(matches!(
        sim.private.update(&[], &[], &cx),
        Err(KeyUpdateError::NothingToDo)
    ));
    let v = adversary_violations(&sim);
    assert!(v.is_empty(), "{v:?}");
}

#[test]
fn committer_state_debug_hides_keys() {
    let mut sim = Sim::new(61);
    sim.join(10);
    let s = format!(
        "{:?} {:?} {:?}",
        sim.crew,
        sim.cover,
        sim.g[0].members.values().next().expect("멤버")
    );
    let t = hex::encode(sim.crew.t().0);
    assert!(!s.contains(&t[..16]));
    assert!(!s.contains("Secret32("));
    let Enc::Direct(m) = &sim.crew.enc else {
        panic!("직접")
    };
    assert_eq!(m.len(), 10);
}

/// 넘겨받지 못한 새 확정 기기: 공개 정보(사슬 끝·멤버)로 이어받아 전체 재키부터 합니다.
#[test]
fn lost_committer_recovers_from_public_tip() {
    let mut sim = Sim::new(71);
    sim.join(330);
    sim.remove(2);
    sim.toggle(4, true);
    let tip = |sim: &Sim, g: usize| {
        let recs = &sim.g[g].recs;
        let generation = recs
            .iter()
            .rev()
            .find_map(|r| match r.p.kind {
                KeyUpdateKind::FullRekey { generation, .. } => Some(generation),
                _ => None,
            })
            .unwrap_or(0);
        ChainTip::after(&recs.last().expect("기록").p, generation)
    };
    let (crew_tip, private_tip, cover_tip) = (tip(&sim, 0), tip(&sim, 1), tip(&sim, 2));
    assert_eq!(crew_tip.generation, sim.crew.generation());
    let space = sim.env.space;
    let crew_devs = sim.crew.devices();
    let private_devs = sim.private.devices();
    let cover_members = sim.cover.members().clone();
    sim.crew = KeyTree::recover(
        space,
        *sim.crew.group(),
        GroupKind::Crew,
        crew_tip,
        &crew_devs,
    );
    sim.private = KeyTree::recover(
        space,
        *sim.private.group(),
        GroupKind::Private,
        private_tip,
        &private_devs,
    );
    sim.cover = CoverGroup::recover(space, *sim.cover.group(), cover_tip, &cover_members);
    let cx = ctx(&sim.env);
    assert!(matches!(
        sim.crew.update(&[], &[dev(9_999)], &cx),
        Err(KeyUpdateError::NeedFullRekey)
    ));
    assert!(matches!(
        sim.crew.refresh_r(&cx),
        Err(KeyUpdateError::NeedFullRekey)
    ));
    assert!(matches!(
        sim.cover.refresh_r(&BTreeSet::new(), &cx),
        Err(KeyUpdateError::NeedFullRekey)
    ));
    let out = sim.crew.full_rekey(&[], &[], &cx).expect("전체 재키");
    assert_eq!(sim.crew.generation(), crew_tip.generation + 1);
    assert!(sim.crew.is_tree());
    sim.after_crew(out, true);
    sim.join(3);
    sim.remove(2);
    sim.rrefresh();
    let v = adversary_violations(&sim);
    assert!(v.is_empty(), "{v:?}");
}

// ---- 검토에서 나온 시험 ----

/// 넘기기 대신 내부로 큰 크루 트리를 만듭니다(기기마다 X-Wing 봉인을 하지 않으려고). 잎은 leaf_master에서, 노드는
/// 무작위입니다.
fn big_crew(space: SpaceId, n: u32) -> KeyTree {
    use super::tree::{Tree, depth_for};
    let lm = Secret32::random();
    let mut crew = KeyTree::crew(space);
    let group = *crew.group();
    let depth = depth_for(n as usize);
    let mut tree = Tree::new(depth, Some(lm.clone()));
    for i in 0..n {
        let d = dev(i + 1);
        let s = tree.take_slot();
        let leaf = Node {
            key: leaf_key(&lm, &group, &d, 1),
            version: 1,
        };
        tree.place(d, s, leaf);
    }
    for l in 1..=depth {
        for p in 0..(n as usize).div_ceil(1 << l) as u32 {
            let node = Node {
                key: Secret32::random(),
                version: 1,
            };
            tree.set(NodeIdx { level: l, pos: p }, Some(node));
        }
    }
    crew.t = tree.get(tree.root()).expect("뿌리").key.clone();
    crew.generation = 1;
    crew.seq = 1;
    crew.root_commit = keysched::root_commit(&space, &group, 1, &crew.t, &crew.r);
    crew.enc = Enc::Tree(Box::new(tree));
    crew
}

/// 3만여 대 크루에서: 덮개 그룹을 처음 만드는 갱신은 목록 없는 `Refresh`(조각 하나)이고, 비공개 트리에 3만 대가 한
/// 번에 들어오는 `Advance`는 가입 목록·칸·감싸기를 여러 조각에 나눠 실어 모두 한도 안이며 합치면 원래대로입니다.
#[test]
fn huge_groups_split_under_the_part_cap() {
    let space = SpaceId([7; 16]);
    let n = 31_000u32;
    let crew = big_crew(space, n);
    let boxes: BTreeMap<DeviceId, BoxPublicKey> = BTreeMap::new();
    let cx = CommitCtx {
        boxes: &boxes,
        crew: Some(&crew),
    };
    let mut cover = CoverGroup::new(space, KeyGroupId::private(&space, b"cover"));
    let excluded: BTreeSet<DeviceId> = [dev(5), dev(20_000)].into_iter().collect();
    let out = cover.update(&excluded, &cx).expect("덮개").expect("바뀜");
    assert!(matches!(out.payload.kind, KeyUpdateKind::Refresh));
    assert_eq!(out.payload.split(PART_TARGET_LEN).expect("나누기").len(), 1);
    assert_eq!(cover.members().len(), n as usize - 2);

    let mut private = KeyTree::private(space, KeyGroupId::private(&space, b"p"));
    let first: Vec<DeviceId> = (1..=300).map(dev).collect();
    private.update(&[], &first, &cx).expect("비공개 만들기");
    assert!(private.is_tree());
    let rest: Vec<DeviceId> = (301..=n).map(dev).collect();
    let out = private.update(&[], &rest, &cx).expect("3만 대 가입");
    assert_eq!(out.payload.kind.joined().len(), rest.len());
    let parts = out.payload.clone().split(PART_TARGET_LEN).expect("나누기");
    assert!(parts.len() >= 2, "조각 {}", parts.len());
    let mut decoded = Vec::new();
    for q in &parts {
        let b = q.encode();
        assert!(b.len() <= PART_TARGET_LEN, "조각 {}B", b.len());
        decoded.push(KeyUpdatePayload::decode(&b).expect("조각 디코딩"));
    }
    assert_eq!(
        KeyUpdatePayload::merge(decoded).expect("합치기"),
        out.payload
    );
}

/// 다른 멤버가 가입자에게 심은 묶음(공개 AD로 봉인했지만 `bundle_root`에 없음)은 열지 않습니다.
#[test]
fn planted_bundle_is_rejected() {
    let mut sim = Sim::new(33);
    sim.join(300);
    let mut jk = sim.fresh(2);
    jk.sort();
    let (j, k) = (jk[0], jk[1]);
    let out = sim.crew.update(&[], &jk, &ctx(&sim.env)).expect("가입");
    let p = &out.payload;
    let (space, group) = (sim.env.space, *sim.crew.group());
    let mine = |d: DeviceId| out.bundles.iter().find(|b| b.device == d).expect("묶음");
    let join = |b: &DeviceBundle| {
        let cx = MemberCtx {
            box_keys: Some(sim.env.kp(&j)),
            bundle: Some(b),
            crew: None,
        };
        MemberKeys::join(space, group, GroupKind::Crew, j, p, &cx)
    };
    // 가짜 잎·경로를 공개 AD로 봉인하고 진짜 묶음의 자리·증명을 베낌.
    let fake = Bundle::Tree {
        slot: sim.crew.slot_of(&j).expect("칸"),
        generation: sim.crew.generation(),
        leaf: Secret32::random(),
        path: vec![Secret32::random(); usize::from(p.depth)],
        r: None,
    };
    let env = seal_to(
        &[(j, sim.env.boxes[&j].clone())],
        &bundle_ad(&space, &group, p.seq, &j, &p.root_commit),
        &fake.encode(),
    );
    let planted = DeviceBundle {
        envelope: env.clone(),
        ..mine(j).clone()
    };
    assert!(matches!(join(&planted), Err(KeyUpdateError::BadBundle)));
    // 증명을 지어내도(자기 봉투 하나짜리 머클 트리) 마찬가지.
    let (_, own) = bundle_tree(vec![(j, env)]);
    assert!(matches!(join(&own[0]), Err(KeyUpdateError::BadBundle)));
    // 다른 기기의 진짜 묶음을 내 것이라 해도 안 됨(잎 해시가 기기 ID를 묶음).
    let relabeled = DeviceBundle {
        device: j,
        ..mine(k).clone()
    };
    assert!(matches!(join(&relabeled), Err(KeyUpdateError::BadBundle)));
    // 전송 인코딩을 거친 진짜 묶음은 열림.
    let real = DeviceBundle::decode(&mine(j).encode()).expect("묶음 디코딩");
    assert_eq!(join(&real).expect("진짜 묶음").t(), sim.crew.t());
}

/// 같은 순번 다른 갈래의 묶음은 증명에서 걸러집니다(확정 기기의 부정 증거인 `RootCommitMismatch`로 보이지 않음).
#[test]
fn bundle_from_other_branch_is_rejected() {
    let mut sim = Sim::new(8);
    sim.join(300);
    let mut b = sim.crew.clone();
    let mut jk = sim.fresh(2);
    jk.sort();
    let (j, k) = (jk[0], jk[1]);
    let cx = ctx(&sim.env);
    let out_a = sim.crew.update(&[], &[j], &cx).expect("A");
    let out_b = b.update(&[], &[j, k], &cx).expect("B");
    assert_eq!(out_a.payload.seq, out_b.payload.seq);
    assert_eq!(sim.crew.slot_of(&j), b.slot_of(&j));
    let (space, group) = (sim.env.space, *sim.crew.group());
    let bundle_of = |o: &KeyUpdateOutput| o.bundles.iter().find(|x| x.device == j).cloned();
    let join = |p: &KeyUpdatePayload, bundle: &DeviceBundle| {
        let cx = MemberCtx {
            box_keys: Some(sim.env.kp(&j)),
            bundle: Some(bundle),
            crew: None,
        };
        MemberKeys::join(space, group, GroupKind::Crew, j, p, &cx)
    };
    let (ba, bb) = (
        bundle_of(&out_a).expect("A 묶음"),
        bundle_of(&out_b).expect("B 묶음"),
    );
    assert!(matches!(
        join(&out_b.payload, &ba),
        Err(KeyUpdateError::BadBundle)
    ));
    assert!(matches!(
        join(&out_a.payload, &bb),
        Err(KeyUpdateError::BadBundle)
    ));
    assert_eq!(join(&out_a.payload, &ba).expect("A").t(), sim.crew.t());
    assert_eq!(join(&out_b.payload, &bb).expect("B").t(), b.t());
}

/// 확정 기기가 가입자에게만 다른 R을 줘도(묶음은 `bundle_root`에 넣음) 뿌리 약속에 R이 들어 있어 드러납니다.
#[test]
fn joiner_with_a_different_r_is_caught() {
    // 트리: 묶음 안의 R.
    let mut sim = Sim::new(31);
    sim.join(300);
    let j = sim.fresh(1)[0];
    let out = sim.crew.update(&[], &[j], &ctx(&sim.env)).expect("가입");
    let (space, group) = (sim.env.space, *sim.crew.group());
    let kp = sim.env.kp(&j);
    let mut p = out.payload.clone();
    let ad = bundle_ad(&space, &group, p.seq, &j, &p.root_commit);
    let pt = open_sealed(&j, kp, &ad, &out.bundles[0].envelope).expect("봉투");
    let Bundle::Tree {
        slot,
        generation,
        leaf,
        path,
        r,
    } = Bundle::decode(&pt).expect("묶음")
    else {
        panic!("트리 묶음이어야 함")
    };
    assert_eq!(r.as_ref(), Some(sim.crew.r()));
    let fake = Bundle::Tree {
        slot,
        generation,
        leaf,
        path,
        r: Some(Secret32::random()),
    };
    let env = seal_to(&[(j, sim.env.boxes[&j].clone())], &ad, &fake.encode());
    let (root, forged) = bundle_tree(vec![(j, env)]);
    p.bundle_root = root;
    let cx = MemberCtx {
        box_keys: Some(kp),
        bundle: Some(&forged[0]),
        crew: None,
    };
    assert!(matches!(
        MemberKeys::join(space, group, GroupKind::Crew, j, &p, &cx),
        Err(KeyUpdateError::RootCommitMismatch)
    ));

    // 직접: 가입 상자 안의 R.
    let mut env = Env::new();
    let devs: Vec<DeviceId> = (1..=5).map(dev).collect();
    for d in &devs {
        env.add(*d);
    }
    let mut crew = KeyTree::crew(env.space);
    crew.update(&[], &devs[..4], &ctx(&env)).expect("만들기");
    let out = crew.update(&[], &devs[4..], &ctx(&env)).expect("가입");
    let group = *crew.group();
    let cx = MemberCtx {
        box_keys: Some(env.kp(&devs[4])),
        ..MemberCtx::default()
    };
    let ok = MemberKeys::join(
        env.space,
        group,
        GroupKind::Crew,
        devs[4],
        &out.payload,
        &cx,
    )
    .expect("진짜 상자");
    assert_eq!(ok.r(), crew.r());
    let mut p = out.payload.clone();
    let fake = Bundle::Direct {
        t: crew.t().clone(),
        r: Some(Secret32::random()),
    };
    p.sealed = vec![SealedBox(seal_to(
        &[(devs[4], env.boxes[&devs[4]].clone())],
        &direct_ad(&env.space, &group, p.seq, &p.root_commit),
        &fake.encode(),
    ))];
    assert!(matches!(
        MemberKeys::join(env.space, group, GroupKind::Crew, devs[4], &p, &cx),
        Err(KeyUpdateError::RootCommitMismatch)
    ));
}

fn rejected(r: Result<(Vec<KeyTree>, Vec<CoverGroup>)>, why: &str) {
    match r {
        Err(KeyUpdateError::BadHandover(m)) => assert_eq!(m, why),
        Err(e) => panic!("{why}: 다른 오류 {e}"),
        Ok(_) => panic!("{why}: 받아들임"),
    }
}

fn tree_mut(k: &mut KeyTree) -> &mut super::tree::Tree {
    match &mut k.enc {
        Enc::Tree(t) => t,
        Enc::Direct(_) => panic!("트리여야 함"),
    }
}

/// 넘기기: 보낸 확정 기기의 서명이 아니거나, 서명은 맞아도 표가 스스로 맞지 않으면 받지 않습니다.
#[test]
fn forged_or_inconsistent_handover_is_rejected() {
    let mut sim = Sim::new(37);
    sim.join(420);
    sim.toggle(5, true);
    assert!(sim.crew.is_tree() && sim.private.is_tree());
    let to_dev = sim.crew_devices()[0];
    let to = vec![(to_dev, sim.env.boxes[&to_dev].clone())];
    let kp = sim.env.kp(&to_dev);
    let (signer, from) = committer_key();
    let open = |h: &SealedHandover| open_handover(&to_dev, kp, h, &from);
    let good =
        seal_handover(&[&sim.crew, &sim.private], &[&sim.cover], &to, &signer).expect("봉인");
    let (trees, covers) = open(&good).expect("진짜 표");
    assert_eq!((trees.len(), covers.len()), (2, 1));

    // 받는 기기 공개키로는 누구나 봉인할 수 있음: 다른 키의 서명.
    let (mallory, _) = committer_key();
    let forged = seal_handover(&[&sim.crew], &[], &to, &mallory).expect("봉인");
    rejected(open(&forged), "보낸 확정 기기의 서명이 아님");
    // 진짜 서명을 다른 블롭·봉투에 붙여도 안 됨.
    let swapped = SealedHandover {
        sig: good.sig.clone(),
        ..forged.clone()
    };
    rejected(open(&swapped), "보낸 확정 기기의 서명이 아님");
    // 보낸 기기를 다르게 알고 있으면 안 됨.
    rejected(
        open_handover(&to_dev, kp, &good, &dev(1)),
        "보낸 확정 기기의 서명이 아님",
    );

    // 서명은 맞지만 표가 맞지 않음.
    let seal1 = |k: &KeyTree| seal_handover(&[k], &[], &to, &signer).expect("봉인");
    let mut bad = sim.crew.clone();
    bad.t = Secret32::random();
    rejected(open(&seal1(&bad)), "뿌리 약속");
    let mut bad = sim.crew.clone();
    bad.r = Secret32::random();
    rejected(open(&seal1(&bad)), "뿌리 약속");
    let mut bad = sim.crew.clone();
    let t = tree_mut(&mut bad);
    let root = t.root();
    t.set(
        root,
        Some(Node {
            key: Secret32::random(),
            version: 1,
        }),
    );
    rejected(open(&seal1(&bad)), "뿌리 키가 T가 아님");
    // 크루 잎은 싣지 않고 leaf_master에서 다시 만들므로, 망가뜨린 잎은 건너가지 못합니다.
    let mut bad = sim.crew.clone();
    let t = tree_mut(&mut bad);
    let slot = t.devices().next().expect("기기").0;
    t.levels[0][slot as usize].as_mut().expect("잎").key = Secret32::random();
    let (back, _) = open(&seal1(&bad)).expect("잎은 다시 만듦");
    let leaf = |k: &KeyTree| {
        k.tree().unwrap().levels[0][slot as usize]
            .clone()
            .unwrap()
            .key
            .0
    };
    assert_eq!(leaf(&back[0]), leaf(&sim.crew));
    let mut bad = sim.crew.clone();
    tree_mut(&mut bad).levels[1][0] = None;
    rejected(open(&seal1(&bad)), "트리 모양");
    let mut bad = sim.private.clone();
    let t = tree_mut(&mut bad);
    let slot = t.devices().next().expect("기기").0;
    t.levels[0][slot as usize].as_mut().expect("잎").key = Secret32::random();
    let h = seal_handover(&[&sim.crew, &bad], &[], &to, &signer).expect("봉인");
    rejected(open(&h), "비공개 잎");
    // 그룹마다 따로 넘기면 크루 없이 열리지만, 받는 쪽이 가진 크루와 맞춰 보면 걸립니다.
    let (alone, _) = open(&seal1(&bad)).expect("크루 없이 열림");
    assert!(matches!(
        check_private_against(&sim.crew, &alone[0]),
        Err(KeyUpdateError::BadHandover("비공개 잎"))
    ));
    let (fine, _) = open(&seal1(&sim.private)).expect("진짜 그룹");
    check_private_against(&sim.crew, &fine[0]).expect("맞는 그룹");
    let mut bad = sim.cover.clone();
    bad.t = Secret32::random();
    let h = seal_handover(&[], &[&bad], &to, &signer).expect("봉인");
    rejected(open(&h), "덮개 그룹 약속");
}

/// 크루 전체 재키(표 보유 기기 정리) 뒤 직접 인코딩 비공개 그룹과 덮개 그룹은 바뀐 기기가 없어도, R만 돌리는
/// 작업에서도 새 T를 냅니다(넘긴 표에 그 T가 들어 있었으므로).
#[test]
fn groups_behind_a_crew_full_rekey_get_a_new_t() {
    let mut sim = Sim::new(43);
    sim.join(30);
    assert!(!sim.crew.is_tree() && !sim.private.is_tree());
    let h = sim.crew_devices()[3];
    sim.make_holder(h);
    for round in 0..2 {
        let (pt, ct) = (sim.private.t().clone(), sim.cover.t().clone());
        // 크루 전체 재키만 하고 비공개·덮개는 아직.
        let out = sim
            .crew
            .full_rekey(&[], &[], &ctx(&sim.env))
            .expect("전체 재키");
        let sites = std::mem::take(&mut sim.crew.last_sites);
        let truth = truth_tree(&sim.crew);
        sim.g[0].deliver(&sim.env, out, sites, truth, None);
        assert!(sim.private.needs_rebuild(&sim.crew));
        let cx = CommitCtx {
            boxes: &sim.env.boxes,
            crew: Some(&sim.crew),
        };
        let out = if round == 0 {
            sim.private.refresh_r(&cx).expect("R 회전")
        } else {
            sim.private.update(&[], &[], &cx).expect("따라잡기")
        };
        assert!(
            matches!(out.payload.kind, KeyUpdateKind::Refresh),
            "{:?}",
            out.payload.kind
        );
        assert_ne!(sim.private.t(), &pt);
        assert!(!sim.private.needs_rebuild(&sim.crew));
        let sites = std::mem::take(&mut sim.private.last_sites);
        let truth = truth_tree(&sim.private);
        let [crew, private, cover] = &mut sim.g;
        private.deliver(&sim.env, out, sites, truth, Some(&crew.members));
        let out = if round == 0 {
            sim.cover.refresh_r(&sim.excluded, &cx).expect("R 회전")
        } else {
            sim.cover
                .update(&sim.excluded, &cx)
                .expect("덮개")
                .expect("따라잡기")
        };
        assert!(matches!(out.payload.kind, KeyUpdateKind::Refresh));
        assert_ne!(sim.cover.t(), &ct);
        let sites = std::mem::take(&mut sim.cover.last_sites);
        let truth = truth_cover(&sim.cover);
        cover.deliver(&sim.env, out, sites, truth, Some(&crew.members));
        sim.make_holder(h);
    }
    let v = adversary_violations(&sim);
    assert!(v.is_empty(), "{v:?}");
}

/// 덮개 갱신은 기대는 크루 상태(순번·약속)를 적으므로, 가입자는 그 순번의 크루 상태로 열고 다른 순번이면
/// `CrewMismatch`입니다(데몬이 순번마다 보관한 크루 상태를 고름).
#[test]
fn cover_joiner_needs_the_crew_state_it_names() {
    let mut sim = Sim::new(29);
    sim.join(400);
    let j = sim.fresh(1)[0];
    sim.excluded.remove(&j);
    let crew_out1 = sim
        .crew
        .update(&[], &[j], &ctx(&sim.env))
        .expect("크루 가입");
    let cx = CommitCtx {
        boxes: &sim.env.boxes,
        crew: Some(&sim.crew),
    };
    let cover_out = sim
        .cover
        .update(&sim.excluded, &cx)
        .expect("덮개")
        .expect("바뀜");
    assert_eq!(
        cover_out.payload.crew,
        Some(CrewRef {
            seq: sim.crew.seq(),
            root_commit: *sim.crew.root_commit(),
        })
    );
    let k = sim.fresh(1)[0];
    let crew_out2 = sim
        .crew
        .update(&[], &[k], &ctx(&sim.env))
        .expect("크루 가입 2");
    let kp = sim.env.kp(&j);
    let bj = crew_out1
        .bundles
        .iter()
        .find(|b| b.device == j)
        .expect("묶음");
    let jc1 = MemberKeys::join(
        sim.env.space,
        *sim.crew.group(),
        GroupKind::Crew,
        j,
        &crew_out1.payload,
        &MemberCtx {
            box_keys: Some(kp),
            bundle: Some(bj),
            crew: None,
        },
    )
    .expect("크루 가입");
    let jc2 = jc1
        .apply(
            &crew_out2.payload,
            &MemberCtx {
                box_keys: Some(kp),
                ..MemberCtx::default()
            },
        )
        .expect("크루 갱신");
    let join_cover = |c: &MemberKeys| {
        MemberKeys::join(
            sim.env.space,
            *sim.cover.group(),
            GroupKind::Cover,
            j,
            &cover_out.payload,
            &MemberCtx {
                box_keys: Some(kp),
                bundle: None,
                crew: Some(c),
            },
        )
    };
    assert_eq!(
        join_cover(&jc1).expect("그 순번의 크루 상태").t(),
        sim.cover.t()
    );
    assert!(matches!(
        join_cover(&jc2),
        Err(KeyUpdateError::CrewMismatch { want, have: Some(h) }) if want == jc1.seq() && h == jc2.seq()
    ));
}

/// 받아들여지지 않은 갱신(진 갈래·발행 실패)은 작업 전 복제본으로 되돌리고, 다음 갱신은 멤버가 가진 사슬에 이어집니다.
#[test]
fn rejected_update_rolls_back_to_the_clone() {
    let mut sim = Sim::new(47);
    sim.join(300);
    let saved = sim.crew.clone();
    let victim = sim.crew_devices()[9];
    let lost = sim
        .crew
        .update(&[victim], &[], &ctx(&sim.env))
        .expect("발행 못 한 갱신");
    assert!(!sim.crew.contains(&victim));
    sim.crew = saved;
    assert!(sim.crew.contains(&victim));
    sim.remove(4);
    let won = &sim.g[0].recs.last().expect("기록").p;
    assert_eq!(won.seq, lost.payload.seq);
    assert_ne!(won.root_commit, lost.payload.root_commit);
    let v = adversary_violations(&sim);
    assert!(v.is_empty(), "{v:?}");
}

/// 표지가 같은 가짜 감싸기가 섞여 있어도(겹치면 모두 열어 봄) 멤버는 진짜를 찾습니다.
#[test]
fn colliding_wrap_tags_are_tolerated() {
    let mut sim = Sim::new(53);
    sim.join(300);
    let devs = sim.crew_devices();
    let removed = sim.rng.pick(&devs, 3);
    let out = sim
        .crew
        .update(&removed, &[], &ctx(&sim.env))
        .expect("제거");
    let mut p = out.payload.clone();
    let mut wraps = Vec::with_capacity(p.wraps.len() * 2);
    for w in &p.wraps {
        let mut junk = w.clone();
        junk.ct[0] ^= 1;
        wraps.push(junk);
        wraps.push(w.clone());
    }
    p.wraps = wraps;
    p.validate_merged().expect("한도 안");
    let mut applied = 0;
    for (d, m) in sim.g[0]
        .members
        .iter()
        .filter(|(d, _)| !removed.contains(d))
        .take(40)
    {
        let cx = MemberCtx {
            box_keys: Some(sim.env.kp(d)),
            ..MemberCtx::default()
        };
        assert_eq!(m.apply(&p, &cx).expect("적용").t(), sim.crew.t());
        applied += 1;
    }
    assert_eq!(applied, 40);
}

/// 표 보유 기기가 크루에 남은 채 비공개·덮개 그룹에서 빠지려면 크루 전체 재키가 먼저입니다(보통 갱신은 거절).
#[test]
fn holder_leaving_private_and_cover_needs_crew_full_rekey() {
    let mut sim = Sim::new(3);
    sim.join(450);
    assert!(sim.private.is_tree() && sim.crew.is_tree());
    assert!(sim.private.len() > DIRECT_BACK + 1);
    sim.holder_leaves_groups();
    sim.swap(2, 2);
    let v = adversary_violations(&sim);
    assert!(v.is_empty(), "{v:?}");
}

/// 막는 장치를 건너뛰면(보유 기기 목록을 잊고 비공개 전체 재키로 뺌) 보유 기기가 leaf_master로 새 비공개 T를
/// 끌어냅니다(공격자 모형이 이 공격을 봄).
#[test]
fn holder_leaving_private_without_crew_rekey_is_caught() {
    let mut sim = Sim::new(3);
    sim.join(450);
    let h = sim.private.devices()[5];
    sim.make_holder(h);
    sim.crew.holders.clear();
    sim.private_out.insert(h);
    sim.sync_private(true);
    assert!(!sim.private.contains(&h) && sim.crew.contains(&h));
    let v = adversary_violations(&sim);
    assert!(v.iter().any(|s| s.starts_with("I2: 그룹 1")), "{v:?}");
}

/// 같은 순번 두 갈래(넘기기로 받은 표에서 갈라짐): 멤버는 어느 갈래든 적용하고, 다른 갈래의 다음 갱신은 사슬 불일치,
/// 갈래 A에서 빠진 기기는 A를 적용하지 못합니다.
#[test]
fn members_follow_either_branch_of_the_same_seq() {
    let mut sim = Sim::new(8);
    sim.join(300);
    let to_dev = sim.crew_devices()[0];
    let to = vec![(to_dev, sim.env.boxes[&to_dev].clone())];
    let (signer, from) = committer_key();
    let sealed = seal_handover(&[&sim.crew], &[], &to, &signer).expect("봉인");
    let (mut trees, _) = open_handover(&to_dev, sim.env.kp(&to_dev), &sealed, &from).expect("열기");
    let mut b = trees.pop().expect("크루");
    assert_eq!(b.tip(), sim.crew.tip());
    let devs = sim.crew_devices();
    let x = devs[10];
    let cx = ctx(&sim.env);
    let out_a = sim.crew.update(&[x], &[], &cx).expect("A");
    let out_b = b.refresh_root(&cx).expect("B");
    assert_eq!(out_a.payload.seq, out_b.payload.seq);
    assert_ne!(out_a.payload.root_commit, out_b.payload.root_commit);
    let m = sim.g[0].members[&devs[20]].clone();
    let mcx = MemberCtx {
        box_keys: Some(sim.env.kp(&devs[20])),
        ..MemberCtx::default()
    };
    let a = m.apply(&out_a.payload, &mcx).expect("A 적용");
    let bb = m.apply(&out_b.payload, &mcx).expect("B 적용");
    assert_eq!(a.t(), sim.crew.t());
    assert_eq!(bb.t(), b.t());
    let out_b2 = b.refresh_r(&cx).expect("B2");
    assert!(matches!(
        a.apply(&out_b2.payload, &mcx),
        Err(KeyUpdateError::ChainMismatch)
    ));
    bb.apply(&out_b2.payload, &mcx).expect("B2 적용");
    let mx = sim.g[0].members[&x].clone();
    let xcx = MemberCtx {
        box_keys: Some(sim.env.kp(&x)),
        ..MemberCtx::default()
    };
    assert!(mx.apply(&out_a.payload, &xcx).is_err());
}

/// 가득 찬 트리에서 한 번에 두 단 자람(가입), 그리고 `Rotate` 안에서 자람.
#[test]
fn tree_grows_by_two_levels_and_inside_rotate() {
    let mut sim = Sim::new(13);
    sim.join(512);
    assert_eq!(sim.crew.depth(), 9);
    sim.join(1000);
    assert_eq!(sim.crew.depth(), 11, "두 단 자람");
    let left = 2048 - sim.crew.len();
    sim.join(left);
    assert_eq!((sim.crew.depth(), sim.crew.len()), (11, 2048));
    // 1대 빼고 3대 넣음: 빈 칸 하나를 쓰고 나머지 2대로 자람.
    sim.swap(1, 3);
    assert_eq!(sim.crew.depth(), 12);
    sim.remove(5);
    sim.refresh();
    let v = adversary_violations(&sim);
    assert!(v.is_empty(), "{v:?}");
}

/// 모든 기기를 빼고 같은 수를 넣는 `Rotate`(옛 뿌리 아래가 모두 비었다가 다시 참).
#[test]
fn rotate_can_replace_everyone() {
    let mut sim = Sim::new(17);
    sim.join(300);
    let devs = sim.crew_devices();
    let joined = sim.fresh(devs.len());
    sim.crew_update(&devs, &joined);
    assert!(sim.crew.is_tree());
    let v = adversary_violations(&sim);
    assert!(v.is_empty(), "{v:?}");
}

/// 망가진 페이로드를 적용해도 패닉하지 않습니다(오류만).
#[test]
fn apply_is_total_under_mutation() {
    let mut sim = Sim::new(23);
    sim.join(450);
    sim.swap(3, 4);
    sim.toggle(5, true);
    sim.toggle(5, false);
    let mut rng = Rng(99);
    let mut panics = Vec::new();
    for g in 0..3usize {
        let recs: Vec<KeyUpdatePayload> = sim.g[g].recs.iter().map(|r| r.p.clone()).collect();
        let states: Vec<(DeviceId, MemberKeys)> = sim.g[g]
            .states
            .iter()
            .take(60)
            .map(|(d, m)| (*d, m.clone()))
            .collect();
        for round in 0..600 {
            let mut p = recs[rng.next() as usize % recs.len()].clone();
            for _ in 0..(1 + rng.next() % 3) {
                match rng.next() % 13 {
                    0 => p.depth = (rng.next() % 18) as u8,
                    1 => p.seq = rng.next() % 12,
                    2 => {
                        p.kind = match rng.next() % 5 {
                            0 => KeyUpdateKind::Refresh,
                            1 => KeyUpdateKind::RRefresh,
                            2 => KeyUpdateKind::FullRekey {
                                generation: rng.next() % 3,
                                removed: Vec::new(),
                                joined: Vec::new(),
                            },
                            3 => KeyUpdateKind::Advance {
                                joined: vec![dev(1)],
                            },
                            _ => KeyUpdateKind::Rotate {
                                removed: vec![],
                                joined: vec![dev(2)],
                            },
                        }
                    }
                    3 if p.depth > 0 => {
                        let s = (rng.next() % (1u64 << p.depth)) as u32;
                        p.layout.push(if rng.next().is_multiple_of(2) {
                            LayoutOp::Clear { slot: s }
                        } else {
                            LayoutOp::Fill {
                                slot: s,
                                device: dev(s + 7),
                            }
                        });
                    }
                    4 => p.layout.clear(),
                    5 => {
                        let n = p.wraps.len();
                        if n > 0 {
                            p.wraps.truncate(rng.next() as usize % n);
                        }
                    }
                    6 => p.wraps.iter_mut().for_each(|w| w.tag = [0; 8]),
                    7 => p.r_ct = None,
                    8 => p.sealed = vec![SealedBox(vec![2, 1, 0])],
                    9 => p.salt[0] ^= 1,
                    10 => {
                        if let Some(r) = &mut p.r_ct {
                            r.ct[0] ^= 1;
                        }
                    }
                    11 => {
                        p.crew = match p.crew {
                            Some(c) => (rng.next().is_multiple_of(2)).then_some(CrewRef {
                                seq: c.seq + 1,
                                ..c
                            }),
                            None => Some(CrewRef {
                                seq: 1,
                                root_commit: [0; 32],
                            }),
                        }
                    }
                    _ => p.prev_root_commit[0] ^= 1,
                }
            }
            let (d, m) = &states[rng.next() as usize % states.len()];
            let cx = MemberCtx {
                box_keys: Some(sim.env.kp(d)),
                bundle: None,
                crew: sim.g[0].members.get(d),
            };
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = m.apply(&p, &cx);
                let _ = MemberKeys::join(sim.env.space, sim.g[g].group, sim.g[g].kind, *d, &p, &cx);
            }));
            if res.is_err() {
                panics.push((
                    g,
                    round,
                    format!("{:?} 깊이 {} 순번 {}", p.kind, p.depth, p.seq),
                ));
            }
        }
    }
    assert!(
        panics.is_empty(),
        "패닉 {}개, 처음 {:?}",
        panics.len(),
        panics.first()
    );
}

// ───────────── 저장 인코딩·종류 추론·이웃 칸 ─────────────

/// 사용자를 아는 상자 키 표(이웃 칸 시험용).
struct UserBoxes {
    boxes: BTreeMap<DeviceId, BoxPublicKey>,
    users: BTreeMap<DeviceId, yangpa_core::ids::UserId>,
}

impl BoxKeys for UserBoxes {
    fn box_key(&self, device: &DeviceId) -> Option<&BoxPublicKey> {
        self.boxes.get(device)
    }

    fn user_of(&self, device: &DeviceId) -> Option<yangpa_core::ids::UserId> {
        self.users.get(device).copied()
    }
}

#[test]
fn member_keys_and_tables_survive_encoding() {
    let mut env = Env::new();
    let devs: Vec<DeviceId> = (0..3).map(dev).collect();
    for d in &devs {
        env.add(*d);
    }
    let cx = CommitCtx {
        boxes: &env.boxes,
        crew: None,
    };
    let mut crew = KeyTree::crew(env.space);
    let out = crew.update(&[], &devs, &cx).unwrap();
    let mcx = MemberCtx {
        box_keys: Some(env.kp(&devs[0])),
        bundle: None,
        crew: None,
    };
    let m = MemberKeys::join(
        env.space,
        KeyGroupId::crew(&env.space),
        GroupKind::Crew,
        devs[0],
        &out.payload,
        &mcx,
    )
    .unwrap();
    let back = MemberKeys::decode(&m.encode()).unwrap();
    assert_eq!(back.root_commit(), m.root_commit());
    assert_eq!(back.state_key().0, m.state_key().0);
    // 약속과 맞지 않게 망가진 상태는 풀지 않습니다.
    let mut bad = m.encode().to_vec();
    let n = bad.len();
    bad[n - 2] ^= 1;
    assert!(MemberKeys::decode(&bad).is_err());

    let me = devs[0];
    let table = encode_table(&[&crew], &[]);
    let (trees, covers) = decode_table(&table, &me).unwrap();
    assert!(covers.is_empty());
    assert_eq!(trees[0].root_commit(), crew.root_commit());
    assert_eq!(trees[0].devices(), crew.devices());
}

/// 크루 트리 표는 잎(기기마다 32바이트 키)을 싣지 않아, 잎이 노드 절반인 트리에서 표가 그만큼 작습니다.
#[test]
fn crew_table_omits_derivable_leaves() {
    let mut sim = Sim::new(53);
    sim.join(600);
    assert!(sim.crew.is_tree());
    let n = sim.crew_devices().len();
    let me = sim.crew_devices()[0];
    let bytes = encode_table(&[&sim.crew], &[]);
    // 잎까지 실으면 기기마다 키 32바이트가 더 듭니다.
    let leaves: usize = sim.crew.tree().unwrap().levels[0].iter().flatten().count();
    assert_eq!(leaves, n);
    let internal: usize = sim.crew.tree().unwrap().levels[1..]
        .iter()
        .map(|l| l.iter().flatten().count())
        .sum();
    assert!(
        bytes.len() < (internal + n / 2) * 34 + n * 34,
        "{}",
        bytes.len()
    );
    let (trees, _) = decode_table(&bytes, &me).unwrap();
    let a = trees[0].tree().unwrap();
    let b = sim.crew.tree().unwrap();
    for (l, (x, y)) in a.levels.iter().zip(&b.levels).enumerate() {
        for (p, (x, y)) in x.iter().zip(y).enumerate() {
            assert_eq!(
                x.as_ref().map(|n| (n.key.0, n.version)),
                y.as_ref().map(|n| (n.key.0, n.version)),
                "노드 ({l},{p})"
            );
        }
    }
}

/// 공개 사슬이 그룹이 크루 전체 재키 뒤에 머물렀다고 말하면 바뀐 기기가 없어도 다음 작업이 새 T를 냅니다.
#[test]
fn marked_stale_groups_issue_a_new_t() {
    let mut sim = Sim::new(59);
    sim.join(30);
    let cx = CommitCtx {
        boxes: &sim.env.boxes,
        crew: Some(&sim.crew),
    };
    let t0 = sim.private.t().0;
    let mut p = sim.private.clone();
    assert!(matches!(
        p.update(&[], &[], &cx),
        Err(KeyUpdateError::NothingToDo)
    ));
    p.mark_stale();
    let out = p.update(&[], &[], &cx).expect("새 T");
    assert!(out.payload.kind.new_r());
    assert_ne!(p.t().0, t0);
    let mut c = sim.cover.clone();
    let excluded = sim.excluded.clone();
    assert!(c.update(&excluded, &cx).expect("갱신").is_none());
    c.mark_stale();
    assert!(c.update(&excluded, &cx).expect("갱신").is_some());
}

#[test]
fn private_member_follows_switch_between_tree_and_cover_payloads() {
    // 직접 인코딩에서 봉인 상자뿐인 갱신은 덮개·비공개가 같은 계산이라 앞 종류를 그대로 씁니다.
    let mut env = Env::new();
    let devs: Vec<DeviceId> = (0..3).map(dev).collect();
    for d in &devs {
        env.add(*d);
    }
    let cx = CommitCtx {
        boxes: &env.boxes,
        crew: None,
    };
    let mut crew = KeyTree::crew(env.space);
    crew.update(&[], &devs, &cx).unwrap();
    let g = KeyGroupId([9; 16]);
    let pcx = CommitCtx {
        boxes: &env.boxes,
        crew: Some(&crew),
    };
    let mut cover = CoverGroup::new(env.space, g);
    let out = cover
        .update(&BTreeSet::from([devs[2]]), &pcx)
        .unwrap()
        .expect("첫 갱신");
    let mcx = MemberCtx {
        box_keys: Some(env.kp(&devs[0])),
        bundle: None,
        crew: None,
    };
    let m = MemberKeys::join(
        env.space,
        g,
        GroupKind::Private,
        devs[0],
        &out.payload,
        &mcx,
    )
    .unwrap();
    assert_eq!(m.t().0, cover.t().0);
}

#[test]
fn a_users_devices_land_in_neighbouring_slots() {
    let mut env = Env::new();
    let n = DIRECT_MAX as u32 + 4;
    let devs: Vec<DeviceId> = (0..n).map(dev).collect();
    for d in &devs {
        env.add(*d);
    }
    // 사용자 u는 처음 기기 하나, 나중에 두 대를 더 넣습니다(ID가 멀어도 이웃 칸으로).
    let u = yangpa_core::ids::UserId([5; 32]);
    let mut users = BTreeMap::new();
    for d in [devs[3], devs[n as usize - 1], devs[n as usize - 2]] {
        users.insert(d, u);
    }
    let boxes = UserBoxes {
        boxes: env.boxes.clone(),
        users,
    };
    let cx = CommitCtx {
        boxes: &boxes,
        crew: None,
    };
    let mut crew = KeyTree::crew(env.space);
    crew.update(&[], &devs[..n as usize - 2], &cx).unwrap();
    assert!(crew.is_tree());
    crew.update(&[], &devs[n as usize - 2..], &cx).unwrap();
    let s0 = crew.slot_of(&devs[3]).unwrap();
    for d in &devs[n as usize - 2..] {
        let s = crew.slot_of(d).unwrap();
        assert_eq!(s >> 3, s0 >> 3, "같은 8칸 블록");
    }
}
