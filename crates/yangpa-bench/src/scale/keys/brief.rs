//! 원안: 키 그룹을 MLS 하위 그룹(기기 256대 상한)으로 나누고, 그룹 키는 하위 그룹 상자 키로 봉인합니다.
//!
//! - 하위 그룹 배정은 BLAKE3(키 그룹 ‖ 사용자)의 확장 해싱입니다. 넘치면 그 칸만 다음 비트로 나눕니다(합치지 않음).
//! - 하위 그룹 커밋은 그 하위 그룹의 일반 멤버 기기가 바로 냅니다(경로 있음, 기기당 약 1.24KB, 웰컴 약 1.48KB).
//!   MLS 레인이라 공간 스트림에 실려 모든 크루 기기가 받습니다.
//! - 제거: 크루 확정 기기가 묶음 창(300ms, 제거가 이어지면 2초까지) 뒤 새 그룹 키를 하위 그룹마다 X-Wing으로
//!   감싼 묶음(1,227B × 하위 그룹 수)을 일반 이벤트 하나로 냅니다.
//! - 가입: 그룹 키를 KDF로 바로 전진합니다. 기다리는 제거 회전보다 먼저 나갈 수 있어, 그 사이 제거된 기기가
//!   전진한 키를 계산합니다(검사기가 I2로 잡아야 하는 원안의 결함).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::{CostClass, Cx, KeyScheme, Lane, OpCost, Pending, Reach, Scope};
use crate::scale::keys::KeyId;
use crate::scale::trace::{CREW, Delta, Dev, Event, Group, Op, User};

#[derive(Clone, Debug)]
struct Bucket {
    local_depth: u32,
    users: BTreeMap<User, Vec<Dev>>,
    devices: usize,
    epoch: KeyId,
}

impl Bucket {
    fn members(&self) -> impl Iterator<Item = Dev> + '_ {
        self.users.values().flatten().copied()
    }
}

#[derive(Clone, Debug)]
struct BriefGroup {
    global_depth: u32,
    dir: Vec<usize>,
    buckets: Vec<Bucket>,
    t: KeyId,
    members: usize,
}

fn hash(g: Group, u: User) -> u64 {
    let mut h = blake3::Hasher::new();
    h.update(b"yangpa/brief/subgroup");
    h.update(&g.to_le_bytes());
    h.update(&u.to_le_bytes());
    let b = h.finalize();
    u64::from_be_bytes(b.as_bytes()[..8].try_into().expect("8바이트"))
}

impl BriefGroup {
    fn slot(&self, h: u64) -> usize {
        if self.global_depth == 0 {
            0
        } else {
            (h >> (64 - self.global_depth)) as usize
        }
    }

    fn bucket_of(&self, g: Group, u: User) -> usize {
        self.dir[self.slot(hash(g, u))]
    }
}

#[derive(Clone, Debug, Default)]
pub struct BriefSubgroups {
    groups: BTreeMap<Group, BriefGroup>,
    committer: Dev,
    /// 그룹 키 회전을 기다리는 제거(그룹별).
    pending: Pending,
    /// 처음 구성 때 크루 그룹의 하위 그룹 수·평균·최대.
    initial: Option<(usize, f64, usize)>,
    splits: u64,
    /// 기기 → 사용자(제거된 기기는 상태에서 이미 빠졌으므로 직접 기억).
    owners: HashMap<Dev, User>,
}

impl BriefSubgroups {
    /// MLS 레인·그룹 키 묶음 모두 공간 스트림이라 그 밖의 크루 기기도 받습니다.
    fn reach(cx: &Cx<'_>, members: usize) -> Reach {
        Reach {
            own: members,
            others: cx.world.devices().saturating_sub(members),
            target: 0,
        }
    }

    /// 하위 그룹 하나의 경로 있는 커밋: 새 에포크를 그 하위 그룹 모두에게(새 기기에는 웰컴).
    fn subgroup_commit(
        &mut self,
        cx: &mut Cx<'_>,
        g: Group,
        b: usize,
        joiners: &[Dev],
        c: &mut OpCost,
    ) {
        let m = cx.model;
        let grp = self.groups.get_mut(&g).expect("그룹");
        let members = grp.members;
        let bucket = &mut grp.buckets[b];
        // 하위 그룹 커밋은 그 하위 그룹의 멤버 기기가 냅니다(가장 작은 번호).
        cx.kn.set_issuer(bucket.members().min());
        bucket.epoch = cx.kn.key(Scope::Group(g), true);
        let n = bucket.devices;
        for d in bucket.members() {
            cx.kn.seal(d, bucket.epoch);
        }
        c.xwing += n as u64;
        let reach = Self::reach(cx, members);
        let body = (m.mls_commit_per_device * n as f64) as usize;
        c.event(cx.kn, m, "하위 그룹 커밋", Lane::KeyAgreement, body, reach);
        if !joiners.is_empty() {
            let body = (m.mls_welcome_per_device * n as f64) as usize;
            c.event(cx.kn, m, "하위 그룹 웰컴", Lane::KeyAgreement, body, reach);
        }
        cx.kn.set_issuer(Some(self.committer));
    }

    /// 사용자 하나를 넣습니다(넘치면 나눔).
    fn insert(&mut self, cx: &mut Cx<'_>, g: Group, u: User, devs: &[Dev], c: &mut OpCost) {
        let cap = cx.model.subgroup_cap;
        loop {
            let grp = self.groups.get_mut(&g).expect("그룹");
            let b = grp.bucket_of(g, u);
            let bucket = &mut grp.buckets[b];
            if bucket.devices + devs.len() <= cap || bucket.local_depth >= 32 {
                for d in devs {
                    self.owners.insert(*d, u);
                }
                let grp = self.groups.get_mut(&g).expect("그룹");
                let bucket = &mut grp.buckets[b];
                bucket.users.entry(u).or_default().extend_from_slice(devs);
                bucket.devices += devs.len();
                grp.members += devs.len();
                if c.class != CostClass::Setup {
                    self.subgroup_commit(cx, g, b, devs, c);
                }
                return;
            }
            self.split(cx, g, b, c);
        }
    }

    fn split(&mut self, cx: &mut Cx<'_>, g: Group, b: usize, c: &mut OpCost) {
        self.splits += 1;
        let grp = self.groups.get_mut(&g).expect("그룹");
        let l = grp.buckets[b].local_depth;
        if l == grp.global_depth {
            // 디렉터리를 두 배로.
            grp.dir = grp.dir.iter().flat_map(|x| [*x, *x]).collect();
            grp.global_depth += 1;
        }
        let new = grp.buckets.len();
        let moved: BTreeMap<User, Vec<Dev>> = {
            let old = &mut grp.buckets[b];
            old.local_depth += 1;
            let (stay, go): (BTreeMap<_, _>, BTreeMap<_, _>) = std::mem::take(&mut old.users)
                .into_iter()
                .partition(|(u, _)| (hash(g, *u) >> (63 - l)) & 1 == 0);
            old.users = stay;
            old.devices = old.users.values().map(Vec::len).sum();
            go
        };
        let devices = moved.values().map(Vec::len).sum();
        grp.buckets.push(Bucket {
            local_depth: l + 1,
            users: moved,
            devices,
            epoch: 0,
        });
        let shift = grp.global_depth - (l + 1);
        for (i, slot) in grp.dir.iter_mut().enumerate() {
            if *slot == b && ((i >> shift) & 1) == 1 {
                *slot = new;
            }
        }
        if c.class == CostClass::Setup {
            return;
        }
        // 옛 하위 그룹은 옮긴 기기를 빼는 커밋, 새 하위 그룹은 웰컴으로 만듭니다.
        let mut sc = OpCost::new(CostClass::Split, g);
        self.subgroup_commit(cx, g, b, &[], &mut sc);
        let movers: Vec<Dev> = self.groups[&g].buckets[new].members().collect();
        self.subgroup_commit(cx, g, new, &movers, &mut sc);
        add_cost(c, &sc);
    }

    /// 사용자의 기기를 하위 그룹에서 뺍니다. 커밋은 부른 쪽이 하위 그룹마다 한 번 냅니다(한 이벤트로 여럿을 내보낼
    /// 때 먼저 낸 커밋이 곧 빠질 기기에게 새 에포크를 주지 않게).
    fn remove_user(&mut self, g: Group, u: User, devs: &[Dev]) -> Option<usize> {
        let grp = self.groups.get_mut(&g).expect("그룹");
        let b = grp.bucket_of(g, u);
        let bucket = &mut grp.buckets[b];
        let v = bucket.users.get_mut(&u)?;
        let before = v.len();
        v.retain(|d| !devs.contains(d));
        let gone = before - v.len();
        if v.is_empty() {
            bucket.users.remove(&u);
        }
        bucket.devices -= gone;
        grp.members -= gone;
        (gone > 0).then_some(b)
    }

    /// 그룹 키 회전: 새 키를 하위 그룹 상자 키(에포크에서 나옴)마다 감싼 묶음 하나.
    fn rotate(&mut self, cx: &mut Cx<'_>, g: Group, class: CostClass) -> OpCost {
        let m = cx.model;
        cx.kn.set_issuer(Some(self.committer));
        let grp = self.groups.get_mut(&g).expect("그룹");
        grp.t = cx.kn.key(Scope::Group(g), true);
        let mut subgroups = 0;
        for b in &grp.buckets {
            if b.devices > 0 {
                cx.kn.wrap(b.epoch, grp.t);
                subgroups += 1;
            }
        }
        let t = grp.t;
        let members = grp.members;
        cx.kn.settle(g, t);
        let mut c = OpCost::new(class, g);
        c.xwing = subgroups as u64;
        let body = 64 + m.xwing_seal * subgroups;
        c.event(
            cx.kn,
            m,
            "그룹 키 묶음",
            Lane::Normal,
            body,
            Self::reach(cx, members),
        );
        c
    }

    fn flush(&mut self, cx: &mut Cx<'_>) -> Vec<OpCost> {
        let groups = self.pending.take();
        cx.kn.begin();
        groups
            .iter()
            .map(|(g, pg)| self.rotate(cx, *g, pg.class()))
            .collect()
    }

    /// 그룹별로 (사용자, 기기들)로 묶습니다.
    fn by_user(
        cx: &Cx<'_>,
        list: &[(Group, Dev)],
        users: &HashMap<Dev, User>,
    ) -> BTreeMap<(Group, User), Vec<Dev>> {
        let mut m: BTreeMap<(Group, User), Vec<Dev>> = BTreeMap::new();
        for &(g, d) in list {
            if let Some(u) = cx.world.user_of(d).or_else(|| users.get(&d).copied()) {
                m.entry((g, u)).or_default().push(d);
            }
        }
        m
    }
}

fn add_cost(c: &mut OpCost, x: &OpCost) {
    c.events += x.events;
    c.published += x.published;
    c.own += x.own;
    c.others += x.others;
    c.target += x.target;
    c.total_download += x.total_download;
    c.xwing += x.xwing;
    c.signatures += x.signatures;
    c.max_body = c.max_body.max(x.max_body);
    c.over_cap += x.over_cap;
}

impl KeyScheme for BriefSubgroups {
    fn name(&self) -> &'static str {
        "brief"
    }

    fn init(&mut self, cx: &mut Cx<'_>) -> Vec<OpCost> {
        self.committer = cx.world.committer;
        cx.kn.set_issuer(Some(self.committer));
        let users: Vec<(User, Vec<Dev>)> = cx
            .world
            .user_devices()
            .map(|(u, d)| (u, d.to_vec()))
            .collect();
        let mut out = Vec::new();
        for g in 0..cx.world.groups.len() as Group {
            self.groups.insert(
                g,
                BriefGroup {
                    global_depth: 0,
                    dir: vec![0],
                    buckets: vec![Bucket {
                        local_depth: 0,
                        users: BTreeMap::new(),
                        devices: 0,
                        epoch: 0,
                    }],
                    t: 0,
                    members: 0,
                },
            );
            let mut c = OpCost::new(CostClass::Setup, g);
            let set = cx.world.group(g).clone();
            for (u, devs) in &users {
                let devs: Vec<Dev> = devs.iter().filter(|d| set.contains(d)).copied().collect();
                if !devs.is_empty() {
                    self.insert(cx, g, *u, &devs, &mut c);
                }
            }
            // 하위 그룹마다 에포크(웰컴)를 세우고 그룹 키를 감쌉니다.
            let m = cx.model;
            let grp = self.groups.get_mut(&g).expect("그룹");
            let members = grp.members;
            for b in &mut grp.buckets {
                b.epoch = cx.kn.key(Scope::Group(g), true);
                for d in b.users.values().flatten() {
                    cx.kn.seal(*d, b.epoch);
                }
                c.xwing += b.devices as u64;
                let body = (m.mls_welcome_per_device * b.devices as f64) as usize;
                c.event(
                    cx.kn,
                    m,
                    "하위 그룹 웰컴(처음)",
                    Lane::KeyAgreement,
                    body,
                    Self::reach(cx, members),
                );
            }
            out.push(c);
            if g == CREW {
                let grp = &self.groups[&CREW];
                let n = grp.buckets.len();
                let max = grp.buckets.iter().map(|b| b.devices).max().unwrap_or(0);
                self.initial = Some((n, grp.members as f64 / n.max(1) as f64, max));
            }
            let mut r = self.rotate(cx, g, CostClass::Setup);
            r.class = CostClass::Setup;
            out.push(r);
        }
        out
    }

    fn apply(&mut self, cx: &mut Cx<'_>, ev: &Event, d: &Delta) -> Vec<OpCost> {
        let mut out = Vec::new();
        // 제거: 하위 그룹 커밋은 바로(하위 그룹마다 한 번), 그룹 키 회전은 묶음 창 뒤.
        let mut touched: BTreeSet<(Group, usize)> = BTreeSet::new();
        for ((g, u), devs) in Self::by_user(cx, &d.removed, &self.owners) {
            if let Some(b) = self.remove_user(g, u, &devs) {
                touched.insert((g, b));
            }
        }
        let mut per_group: BTreeMap<Group, OpCost> = BTreeMap::new();
        for (g, b) in touched {
            let c = per_group
                .entry(g)
                .or_insert_with(|| OpCost::new(super::cause_of(&ev.op, false), g));
            self.subgroup_commit(cx, g, b, &[], c);
        }
        out.extend(per_group.into_values());
        self.pending.push(
            &Delta {
                removed: d.removed.clone(),
                ..Default::default()
            },
            &ev.op,
            ev.at_ms,
            cx.model,
            true,
        );
        // 가입: 하위 그룹에 넣고(커밋·웰컴) 그룹 키를 바로 KDF로 전진, 새 기기에 봉인. 한 변동(갈라짐 합치기의
        // 반대쪽 가입 전부 포함)의 가입은 그룹마다 한 번 전진합니다.
        let mut joined: BTreeMap<Group, (OpCost, Vec<Dev>)> = BTreeMap::new();
        for ((g, u), devs) in Self::by_user(cx, &d.added, &self.owners) {
            let (c, all) = joined
                .entry(g)
                .or_insert_with(|| (OpCost::new(super::cause_of(&ev.op, true), g), Vec::new()));
            self.insert(cx, g, u, &devs, c);
            all.extend(devs);
        }
        for (g, (mut c, devs)) in joined {
            let m = cx.model;
            let grp = self.groups.get_mut(&g).expect("그룹");
            grp.t = cx.kn.derive(grp.t, Scope::Group(g), true);
            let (t, members) = (grp.t, grp.members);
            for &dev in &devs {
                cx.kn.seal(dev, t);
            }
            cx.kn.settle(g, t);
            c.xwing += devs.len() as u64;
            c.event(
                cx.kn,
                m,
                "그룹 키 전진",
                Lane::Normal,
                64 + 32 * devs.len(),
                Self::reach(cx, members),
            );
            let body = m.seal(32) * devs.len();
            c.event(
                cx.kn,
                m,
                "가입자 키",
                Lane::Normal,
                body,
                Reach {
                    target: devs.len(),
                    ..Default::default()
                },
            );
            out.push(c);
        }
        if let Some((_, new, _)) = d.committer {
            // 새 확정 기기가 모든 그룹 키를 회전합니다(기다리던 제거도 덮음).
            self.committer = new;
            self.pending.take();
            cx.kn.begin();
            let groups: Vec<Group> = self.groups.keys().copied().collect();
            for g in groups {
                out.push(self.rotate(cx, g, CostClass::Committer));
            }
        }
        out
    }

    fn advance(&mut self, cx: &mut Cx<'_>) -> Vec<OpCost> {
        if self.pending.due(cx.now).is_some() {
            return self.flush(cx);
        }
        Vec::new()
    }

    fn resync(&mut self, cx: &mut Cx<'_>, d: &Delta) -> Vec<OpCost> {
        // 하위 그룹 커밋이 양쪽에서 따로 나가 갈래가 생겼으므로, 이긴 쪽이 변화를 반영한 뒤 모든 하위 그룹을 새
        // 에포크로 다시 세우고 그룹 키를 회전합니다.
        let mut out = self.apply(
            cx,
            &Event {
                at_ms: cx.now,
                side: crate::scale::trace::Side::Both,
                op: Op::PartitionEnd,
            },
            d,
        );
        out.extend(self.flush(cx));
        cx.kn.begin();
        let groups: Vec<Group> = self.groups.keys().copied().collect();
        for g in groups {
            let n = self.groups[&g].buckets.len();
            let mut c = OpCost::new(CostClass::Merge, g);
            for b in 0..n {
                if self.groups[&g].buckets[b].devices > 0 {
                    let all: Vec<Dev> = self.groups[&g].buckets[b].members().collect();
                    self.subgroup_commit(cx, g, b, &all, &mut c);
                }
            }
            add_cost(&mut c, &self.rotate(cx, g, CostClass::Merge));
            out.push(c);
        }
        out
    }

    fn finish(&mut self, cx: &mut Cx<'_>) -> Vec<OpCost> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        self.flush(cx)
    }

    fn boxed_clone(&self) -> Box<dyn KeyScheme + Send> {
        Box::new(self.clone())
    }

    fn notes(&self) -> Vec<(String, String)> {
        let mut v = Vec::new();
        if let Some((n, mean, max)) = self.initial {
            v.push(("크루 하위 그룹 수(처음)".into(), n.to_string()));
            v.push(("하위 그룹 평균 기기(처음)".into(), format!("{mean:.0}")));
            v.push(("하위 그룹 최대 기기(처음)".into(), max.to_string()));
        }
        if let Some(g) = self.groups.get(&CREW) {
            let live: Vec<usize> = g
                .buckets
                .iter()
                .map(|b| b.devices)
                .filter(|n| *n > 0)
                .collect();
            v.push(("크루 하위 그룹 수(끝)".into(), live.len().to_string()));
        }
        let all: BTreeSet<usize> = self.groups.values().map(|g| g.buckets.len()).collect();
        v.push(("그룹별 하위 그룹 수".into(), format!("{all:?}")));
        v.push(("나누기".into(), self.splits.to_string()));
        v
    }
}
