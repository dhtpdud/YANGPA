//! 수정판: 키 그룹마다 확정 기기 한 대가 `KeyUpdate`(코드 82, 960KiB 한도) 사슬을 씁니다.
//!
//! - 직접 인코딩(기기 256대 이하): 회전이면 새 T를 남은 기기마다 X-Wing으로 봉인(1,227B씩), 가입이면
//!   T' = KDF(T)를 가입 기기에만 봉인합니다.
//! - 트리 인코딩(256대 초과, 160대 이하가 되면 직접으로): LKH 이진 트리. 확정 기기가 가장 왼쪽 빈 칸에 넣고, 가득
//!   차면 위에 뿌리를 하나 얹습니다. 제거는 제거 경로의 노드를 새로 뽑아 남은 자식 키로 감쌉니다(감싸기 80B).
//!   가입은 경로 노드를 KDF로 한 방향 전진하고 가입 기기에 잎과 경로를 봉인합니다. 크루 잎 키는 leaf_master에서,
//!   비공개 그룹 잎은 크루 잎에서 나와 비공개 그룹에 넣을 때 X-Wing이 필요 없습니다.
//! - 표 보유 기기(leaf_master를 가진 기기: 마지막 전체 재키 뒤 확정 기기였던 기기 + 예비 기기)가 빠지거나 강등되거나
//!   어느 키 그룹의 자격이라도 잃으면, 또 넘겨받지 못한 채 확정 기기가 바뀌거나 7일마다, 모든 그룹을 전체 재키합니다.
//!   비공개 그룹 잎은 크루 잎에서, 크루 잎은 leaf_master에서 나오므로 그 그룹만 다시 세워서는 막지 못합니다. 크루 몫은
//!   사용자별 조각으로 나눈 내용 주소 블롭으로 내고 공간 스트림에는 머리만 실어, 기기마다 자기 조각만 받습니다.
//! - 그룹이 비면 T·R을 버리고, 다시 기기가 들어오면 전진 대신 새로 뽑습니다(떠난 기기가 옛 T에서 계산하지 못하게).
//! - 묶음 창: 300ms, 제거가 이어지면 첫 제거부터 2초까지. 제거 회전은 기다리는 가입 전진보다 먼저(같은 이벤트에서)
//!   냅니다.
//! - 키 일정: T(그룹 뿌리)는 가입마다 전진하는 기록 비공개 사슬, R은 제거·확정 기기 변경·시간 회전에만 새로 뽑는
//!   기록 공개 키로 `r_ct`(새 T로 암호화)에 실립니다. 가입자는 지금 R과 체크포인트 이후의 S를 Inbox로 받습니다.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::{CostClass, Cx, KeyScheme, Lane, OpCost, Pending, PendingGroup, Reach, Scope};
use crate::scale::keys::KeyId;
use crate::scale::trace::{CREW, Delta, Dev, Event, Group, Handover, Op, User};

const NONE: KeyId = KeyId::MAX;
const EMPTY: Dev = Dev::MAX;
/// `KeyUpdate` 고정 필드(그룹 16, 순번 8, 앞 32, 종류, root_commit 32, prev_root_commit 32, 길이들).
const KU_FIXED: usize = 130;
/// 가입·제거 기기 하나: 기기 ID 32B + 칸 번호 4B.
const KU_DEVICE: usize = 36;
/// 새 R을 새 T로 암호화한 것(nonce 24 + 키 32 + 태그 16).
const R_CT: usize = 72;
/// 봉인 안의 경로 항목(노드 번호 4 + 키 32).
const PATH_ENTRY: usize = 36;
/// 체크포인트 이후 S 하나(ID 16 + 키 32).
const S_ENTRY: usize = 48;
/// Inbox 사용자 공용 부분: nonce·태그 40 + R 32.
const INBOX_USER: usize = 72;

#[derive(Clone, Debug)]
struct Tree {
    depth: u32,
    slots: Vec<Dev>,
    dev_slot: HashMap<Dev, u32>,
    /// 힙 배치: 1이 뿌리, 잎은 `2^depth + 칸`.
    nodes: Vec<KeyId>,
    free: BTreeSet<u32>,
    used: u32,
}

impl Tree {
    fn empty(depth: u32) -> Self {
        Tree {
            depth,
            slots: vec![EMPTY; 1 << depth],
            dev_slot: HashMap::new(),
            nodes: vec![NONE; 2 << depth],
            free: BTreeSet::new(),
            used: 0,
        }
    }

    fn leaf(&self, s: u32) -> usize {
        (1usize << self.depth) + s as usize
    }

    /// 칸 `s`의 조상(부모부터 뿌리까지).
    fn path(&self, s: u32) -> impl Iterator<Item = usize> {
        let mut i = self.leaf(s);
        std::iter::from_fn(move || {
            i >>= 1;
            (i >= 1).then_some(i)
        })
    }

    fn present(&self, i: usize) -> bool {
        self.nodes[i] != NONE
    }

    /// 깊이를 하나 늘립니다(지금 트리는 왼쪽 아래로). 새 뿌리 자리는 비워 둡니다.
    fn grow(&mut self) {
        let d = self.depth;
        let mut nodes = vec![NONE; 4 << d];
        for (i, k) in self.nodes.iter().enumerate().skip(1) {
            let l = usize::BITS - 1 - i.leading_zeros();
            let p = i - (1 << l);
            nodes[(2 << l) + p] = *k;
        }
        self.nodes = nodes;
        self.slots.resize(2 << d, EMPTY);
        self.depth = d + 1;
    }

    /// 가장 왼쪽 빈 칸. 없으면 None(키우기 필요).
    fn take_slot(&mut self) -> Option<u32> {
        if let Some(s) = self.free.pop_first() {
            return Some(s);
        }
        if (self.used as usize) < self.slots.len() {
            self.used += 1;
            return Some(self.used - 1);
        }
        None
    }
}

#[derive(Clone, Debug)]
struct KGroup {
    members: BTreeSet<Dev>,
    tree: Option<Tree>,
    t: KeyId,
    r: KeyId,
    last_r: u64,
    joins_since_r: u32,
}

#[derive(Clone, Debug, Default)]
struct Stats {
    full_rekeys: u32,
    blob_per_user: Vec<f64>,
    full_device_download: Vec<f64>,
    max_depth: u32,
    mode_switches: u32,
    split_events: u64,
    handover_bytes: u64,
}

/// 검사기가 잡는지 보려고 일부러 망가뜨린 모형(시험 전용).
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Mutant {
    #[default]
    None,
    /// 표 보유 기기가 빠져도 전체 재키하지 않음.
    NoHolderRekey,
    /// 직접 인코딩에서 제거만 있는 묶음은 회전하지 않음.
    NoRemovalRotation,
}

#[derive(Clone, Debug, Default)]
pub struct KeyTreeModel {
    committer: Dev,
    backup: Option<Dev>,
    holders: BTreeSet<Dev>,
    leaf_master: Option<KeyId>,
    /// leaf_master에서 계산되지 않아 봉인으로 준 잎(크루가 직접 인코딩일 때 비공개 트리에 넣은 기기).
    lone_leaves: BTreeSet<KeyId>,
    groups: BTreeMap<Group, KGroup>,
    pending: Pending,
    need_full: bool,
    last_full: u64,
    last_checkpoint: u64,
    events_since_checkpoint: u32,
    r_since_checkpoint: u32,
    stats: Stats,
    #[cfg(test)]
    pub(crate) mutant: Mutant,
}

fn reach(n: usize) -> Reach {
    // 크루 그룹은 공간 스트림, 비공개 그룹은 자기 KeyGroup 스트림: 어느 쪽이든 그 그룹 기기만 받습니다.
    Reach {
        own: n,
        ..Default::default()
    }
}

fn log2_ceil(n: usize) -> u32 {
    if n <= 1 {
        0
    } else {
        usize::BITS - (n - 1).leading_zeros()
    }
}

impl KeyTreeModel {
    fn crew_leaf(&self, d: Dev) -> Option<KeyId> {
        let t = self.groups.get(&CREW)?.tree.as_ref()?;
        t.dev_slot.get(&d).map(|s| t.nodes[t.leaf(*s)])
    }

    /// 새 잎 키. 크루는 leaf_master에서(가입 기기에 봉인은 부르는 쪽), 비공개 그룹은 크루 잎에서.
    fn new_leaf(&mut self, cx: &mut Cx<'_>, g: Group, d: Dev) -> (KeyId, bool) {
        if g == CREW {
            let lm = self
                .leaf_master
                .expect("트리 인코딩이면 leaf_master가 있음");
            return (cx.kn.derive(lm, Scope::Device(d), true), true);
        }
        match self.crew_leaf(d) {
            Some(cl) => (cx.kn.derive(cl, Scope::Device(d), true), false),
            None => {
                // 크루가 직접 인코딩인데 비공개 그룹이 트리인 경우는 크기상 없지만, 있으면 봉인으로 줍니다.
                let k = cx.kn.key(Scope::Device(d), true);
                cx.kn.seal(d, k);
                self.lone_leaves.insert(k);
                (k, true)
            }
        }
    }

    /// 새 R을 지금 T로 감쌉니다.
    fn new_r(&mut self, cx: &mut Cx<'_>, g: Group) {
        let grp = self.groups.get_mut(&g).expect("그룹");
        grp.r = cx.kn.key(Scope::Group(g), false);
        if grp.t != NONE {
            cx.kn.wrap(grp.t, grp.r);
        }
        grp.last_r = cx.now;
        grp.joins_since_r = 0;
        if g == CREW {
            self.r_since_checkpoint += 1;
        }
    }

    fn settle(&self, cx: &mut Cx<'_>, g: Group) {
        let grp = &self.groups[&g];
        if grp.members.is_empty() {
            return;
        }
        cx.kn.settle(g, grp.t);
        cx.kn.settle(g, grp.r);
    }

    /// `KeyUpdate`를 내되 본문이 한도를 넘으면 여러 이벤트로 나눕니다.
    fn publish(
        &mut self,
        c: &mut OpCost,
        cx: &mut Cx<'_>,
        what: &'static str,
        fixed: usize,
        var: usize,
        n: usize,
    ) {
        let room = Lane::KeyAgreement.cap() - fixed - 1024;
        let parts = var.div_ceil(room).max(1);
        if parts > 1 {
            self.stats.split_events += parts as u64 - 1;
        }
        for i in 0..parts {
            let share = var / parts + usize::from(i < var % parts);
            c.event(
                cx.kn,
                cx.model,
                what,
                Lane::KeyAgreement,
                fixed + share,
                reach(n),
            );
        }
    }

    /// 직접 인코딩 회전: 새 T를 모두에게 봉인, 새 R.
    fn direct_rotate(&mut self, cx: &mut Cx<'_>, g: Group, c: &mut OpCost, ids: usize) {
        let grp = self.groups.get_mut(&g).expect("그룹");
        grp.t = cx.kn.key(Scope::Group(g), true);
        for &d in &grp.members {
            cx.kn.seal(d, grp.t);
        }
        let n = grp.members.len();
        self.new_r(cx, g);
        c.xwing += n as u64;
        let var = n * cx.model.seal(32) + R_CT + ids * KU_DEVICE;
        self.publish(c, cx, "KeyUpdate(직접 회전)", KU_FIXED, var, n);
    }

    /// 트리를 처음부터 세웁니다(빈 칸 없이). 잎 봉인 여부는 부르는 쪽이 정합니다. 만든 감싸기 수를 돌려줍니다.
    fn build_tree(&mut self, cx: &mut Cx<'_>, g: Group) -> usize {
        let members: Vec<Dev> = self.groups[&g].members.iter().copied().collect();
        let depth = log2_ceil(members.len()).max(1);
        let mut tree = Tree::empty(depth);
        for (s, &d) in members.iter().enumerate() {
            let (leaf, seal) = self.new_leaf(cx, g, d);
            if seal && g == CREW {
                cx.kn.seal(d, leaf);
            }
            tree.slots[s] = d;
            tree.dev_slot.insert(d, s as u32);
            let i = tree.leaf(s as u32);
            tree.nodes[i] = leaf;
        }
        tree.used = members.len() as u32;
        let mut wraps = 0;
        for i in (1..(1usize << depth)).rev() {
            let kids: Vec<usize> = [2 * i, 2 * i + 1]
                .into_iter()
                .filter(|k| tree.present(*k))
                .collect();
            if kids.is_empty() {
                continue;
            }
            let k = cx.kn.key(Scope::Group(g), true);
            for kid in kids {
                cx.kn.wrap(tree.nodes[kid], k);
                wraps += 1;
            }
            tree.nodes[i] = k;
        }
        self.stats.max_depth = self.stats.max_depth.max(depth);
        let grp = self.groups.get_mut(&g).expect("그룹");
        grp.t = tree.nodes[1];
        grp.tree = Some(tree);
        self.new_r(cx, g);
        wraps
    }

    /// 크루 트리를 새 leaf_master로 세우고, 사용자별 조각 블롭으로 냅니다.
    fn crew_full(&mut self, cx: &mut Cx<'_>, c: &mut OpCost) {
        let lm = cx.kn.key(Scope::Table, true);
        self.leaf_master = Some(lm);
        if let Some(b) = self.backup
            && b != self.committer
        {
            cx.kn.seal(b, lm);
            c.xwing += 1;
            c.direct += cx.model.seal(32) as u64;
        }
        self.build_tree(cx, CREW);
        let m = cx.model;
        let depth = self.groups[&CREW].tree.as_ref().map_or(0, |t| t.depth) as usize;
        let n = self.groups[&CREW].members.len();
        // 사용자별 조각: 기기마다 X-Wing 봉인(잎 + 경로).
        let mut users: BTreeMap<User, usize> = BTreeMap::new();
        for &d in &self.groups[&CREW].members {
            if let Some(u) = cx.world.user_of(d) {
                *users.entry(u).or_default() += 1;
            }
        }
        let per_dev = m.seal(32 + depth * PATH_ENTRY);
        let proof = 32 * log2_ceil(users.len()) as usize;
        let blob: usize = users.values().map(|k| k * per_dev).sum();
        let head = KU_FIXED + 32 + 8 + R_CT;
        c.event(
            cx.kn,
            m,
            "KeyUpdate(전체 재키 머리)",
            Lane::KeyAgreement,
            head,
            reach(n),
        );
        c.published += blob as u64;
        c.xwing += n as u64;
        let mean_shard = blob as f64 / users.len().max(1) as f64;
        c.own += mean_shard as u64 + proof as u64;
        c.total_download += users
            .values()
            .map(|k| (k * (k * per_dev + proof)) as u64)
            .sum::<u64>();
        self.stats.blob_per_user.push(mean_shard);
        self.stats
            .full_device_download
            .push(m.event(head) as f64 + mean_shard + proof as f64);
    }

    /// 비공개 그룹 트리를 다시 세움(잎은 새 크루 잎에서): 감싸기를 블롭으로, 기기는 자기 경로만 받습니다.
    fn private_rebuild(&mut self, cx: &mut Cx<'_>, g: Group, c: &mut OpCost) {
        let wraps = self.build_tree(cx, g);
        let m = cx.model;
        let n = self.groups[&g].members.len();
        let depth = self.groups[&g].tree.as_ref().map_or(0, |t| t.depth) as usize;
        let head = KU_FIXED + 32 + 8 + R_CT;
        c.event(
            cx.kn,
            m,
            "KeyUpdate(비공개 재구성 머리)",
            Lane::KeyAgreement,
            head,
            reach(n),
        );
        let blob = wraps * m.wrap;
        let per_dev = depth * m.wrap + 32 * log2_ceil(wraps) as usize;
        c.published += blob as u64;
        c.own += per_dev as u64;
        c.total_download += (per_dev * n) as u64;
    }

    /// 모든 그룹 전체 재키(지금 멤버십 그대로, 기다리던 변화도 덮음).
    fn full_rekey(&mut self, cx: &mut Cx<'_>, class: CostClass) -> Vec<OpCost> {
        cx.kn.set_issuer(Some(self.committer));
        self.pending.take();
        let mut out = Vec::new();
        let groups: Vec<Group> = self.groups.keys().copied().collect();
        for g in groups {
            let n = cx.world.group(g).len();
            let grp = self.groups.get_mut(&g).expect("그룹");
            grp.members = cx.world.group(g).clone();
            let mut c = OpCost::new(class, g);
            if n == 0 {
                // 빈 그룹은 키를 버립니다. 다음 가입은 새 T·R로 시작합니다.
                grp.tree = None;
                grp.t = NONE;
                grp.r = NONE;
                continue;
            }
            let tree_mode =
                n > cx.model.direct_max || (grp.tree.is_some() && n > cx.model.direct_back);
            if !tree_mode {
                grp.tree = None;
                self.direct_rotate(cx, g, &mut c, 0);
            } else if g == CREW {
                self.crew_full(cx, &mut c);
            } else {
                self.private_rebuild(cx, g, &mut c);
            }
            self.settle(cx, g);
            out.push(c);
        }
        let keep: Vec<Dev> = std::iter::once(self.committer).chain(self.backup).collect();
        cx.kn.reset_table(&keep);
        self.holders = keep.into_iter().collect();
        self.need_full = false;
        self.last_full = cx.now;
        self.stats.full_rekeys += 1;
        out
    }

    /// 트리 제거: 제거 경로의 노드를 새로 뽑아 남은 자식 키로 감쌉니다. 감싸기 수.
    fn tree_remove(&mut self, cx: &mut Cx<'_>, g: Group, removed: &[Dev]) -> usize {
        let tree = self
            .groups
            .get_mut(&g)
            .and_then(|x| x.tree.as_mut())
            .expect("트리");
        let mut affected = BTreeSet::new();
        for d in removed {
            if let Some(s) = tree.dev_slot.remove(d) {
                tree.slots[s as usize] = EMPTY;
                let i = tree.leaf(s);
                tree.nodes[i] = NONE;
                tree.free.insert(s);
                affected.extend(tree.path(s));
            }
        }
        let mut wraps = 0;
        for &i in affected.iter().rev() {
            let kids: Vec<usize> = [2 * i, 2 * i + 1]
                .into_iter()
                .filter(|k| tree.present(*k))
                .collect();
            if kids.is_empty() {
                tree.nodes[i] = NONE;
                continue;
            }
            let k = cx.kn.key(Scope::Group(g), true);
            for kid in kids {
                cx.kn.wrap(tree.nodes[kid], k);
                wraps += 1;
            }
            tree.nodes[i] = k;
        }
        wraps
    }

    /// 트리 가입: 가장 왼쪽 빈 칸에 넣고 경로를 전진합니다. (가입 기기, 경로 키들), 키우며 생긴 감싸기 수.
    fn tree_add(
        &mut self,
        cx: &mut Cx<'_>,
        g: Group,
        joiners: &[Dev],
    ) -> (Vec<(Dev, Vec<KeyId>)>, usize) {
        let mut wraps = 0;
        let mut slots = Vec::with_capacity(joiners.len());
        for &d in joiners {
            let (leaf, seal) = self.new_leaf(cx, g, d);
            if seal && g == CREW {
                cx.kn.seal(d, leaf);
            }
            let tree = self
                .groups
                .get_mut(&g)
                .and_then(|x| x.tree.as_mut())
                .expect("트리");
            let s = match tree.take_slot() {
                Some(s) => s,
                None => {
                    // 가득 참: 위에 뿌리를 얹고 옛 뿌리로 감쌉니다(감싸기 1개).
                    tree.grow();
                    let k = cx.kn.key(Scope::Group(g), true);
                    if tree.nodes[2] != NONE {
                        cx.kn.wrap(tree.nodes[2], k);
                        wraps += 1;
                    }
                    tree.nodes[1] = k;
                    self.stats.max_depth = self.stats.max_depth.max(tree.depth);
                    tree.take_slot().expect("키운 뒤에는 빈 칸이 있음")
                }
            };
            tree.slots[s as usize] = d;
            tree.dev_slot.insert(d, s);
            let i = tree.leaf(s);
            tree.nodes[i] = leaf;
            slots.push((d, s));
        }
        let tree = self
            .groups
            .get_mut(&g)
            .and_then(|x| x.tree.as_mut())
            .expect("트리");
        let mut on_path = BTreeSet::new();
        for &(_, s) in &slots {
            on_path.extend(tree.path(s));
        }
        for &i in &on_path {
            tree.nodes[i] = if tree.nodes[i] != NONE {
                cx.kn.derive(tree.nodes[i], Scope::Group(g), true)
            } else {
                cx.kn.key(Scope::Group(g), true)
            };
        }
        let paths = slots
            .iter()
            .map(|&(d, s)| {
                let mut keys = vec![tree.nodes[tree.leaf(s)]];
                keys.extend(tree.path(s).map(|i| tree.nodes[i]));
                (d, keys)
            })
            .collect();
        (paths, wraps)
    }

    /// 크루 가입자 Inbox: 사용자마다 KeyShare 하나(기기마다 봉인 + 사용자 공용 R·S).
    fn inbox(&mut self, cx: &mut Cx<'_>, c: &mut OpCost, joined: &[(Dev, usize)]) {
        let m = cx.model;
        let mut by_user: BTreeMap<User, Vec<usize>> = BTreeMap::new();
        for &(d, path) in joined {
            if let Some(u) = cx.world.user_of(d) {
                by_user.entry(u).or_default().push(path);
            }
        }
        let user_part = INBOX_USER + self.r_since_checkpoint as usize * S_ENTRY;
        for (u, paths) in by_user {
            let body: usize = paths
                .iter()
                .map(|p| m.seal(32 + 32 + p * PATH_ENTRY))
                .sum::<usize>()
                + user_part;
            c.xwing += paths.len() as u64;
            let devs = cx.world.devices_of(u).len();
            c.event(
                cx.kn,
                m,
                "Inbox KeyShare",
                Lane::Normal,
                body,
                Reach {
                    target: devs,
                    ..Default::default()
                },
            );
        }
    }

    /// 한 그룹의 묶음(제거 먼저, 그다음 가입)을 냅니다.
    fn flush_group(&mut self, cx: &mut Cx<'_>, g: Group, pg: &PendingGroup) -> OpCost {
        let m = cx.model;
        let class = pg.class();
        let mut c = OpCost::new(class, g);
        let grp = self.groups.get_mut(&g).expect("그룹");
        let removed: Vec<Dev> = pg
            .remove
            .iter()
            .filter(|d| grp.members.remove(d))
            .copied()
            .collect();
        let joined: Vec<Dev> = pg
            .add
            .iter()
            .filter(|d| grp.members.insert(**d))
            .copied()
            .collect();
        if removed.is_empty() && joined.is_empty() {
            return c;
        }
        grp.joins_since_r += joined.len() as u32;
        let n = grp.members.len();
        if n == 0 {
            // 마지막 기기가 빠짐: 남은 기기가 없으니 회전할 것도 없지만, 다음 가입이 옛 T에서 전진하면 떠난 기기가
            // 계산하므로 키를 버립니다.
            grp.tree = None;
            grp.t = NONE;
            grp.r = NONE;
            return c;
        }
        let fresh = grp.t == NONE;
        let is_tree = grp.tree.is_some();
        // 인코딩 바꾸기(모두 새로 받음).
        if !is_tree && n > m.direct_max {
            c.class = CostClass::ModeSwitch;
            self.stats.mode_switches += 1;
            if g == CREW {
                self.crew_full(cx, &mut c);
            } else {
                self.private_rebuild(cx, g, &mut c);
            }
            self.settle(cx, g);
            return c;
        }
        if is_tree && n <= m.direct_back {
            c.class = CostClass::ModeSwitch;
            self.stats.mode_switches += 1;
            self.groups.get_mut(&g).expect("그룹").tree = None;
            self.direct_rotate(cx, g, &mut c, removed.len() + joined.len());
            self.settle(cx, g);
            return c;
        }
        if !is_tree {
            #[cfg(test)]
            if self.mutant == Mutant::NoRemovalRotation && joined.is_empty() {
                self.settle(cx, g);
                return c;
            }
            if !removed.is_empty() || fresh {
                // 제거 회전(또는 빈 그룹에 처음 들어옴)에 가입 기기도 들어 있으므로 따로 봉인할 것이 없습니다.
                self.direct_rotate(cx, g, &mut c, removed.len() + joined.len());
                if g == CREW {
                    let list: Vec<(Dev, usize)> = joined.iter().map(|d| (*d, 0)).collect();
                    let r = self.groups[&g].r;
                    for &d in &joined {
                        cx.kn.seal(d, r);
                    }
                    self.inbox(cx, &mut c, &list);
                }
            } else {
                let grp = self.groups.get_mut(&g).expect("그룹");
                grp.t = cx.kn.derive(grp.t, Scope::Group(g), true);
                let (t, r) = (grp.t, grp.r);
                for &d in &joined {
                    cx.kn.seal(d, t);
                    cx.kn.seal(d, r);
                }
                if g == CREW {
                    let fixed = KU_FIXED + joined.len() * KU_DEVICE;
                    self.publish(&mut c, cx, "KeyUpdate(직접 전진)", fixed, 0, n);
                    let list: Vec<(Dev, usize)> = joined.iter().map(|d| (*d, 0)).collect();
                    self.inbox(cx, &mut c, &list);
                } else {
                    c.xwing += joined.len() as u64;
                    let var = joined.len() * (KU_DEVICE + m.seal(64));
                    self.publish(&mut c, cx, "KeyUpdate(직접 전진)", KU_FIXED, var, n);
                }
            }
            self.settle(cx, g);
            return c;
        }
        // 트리 인코딩.
        let mut wraps = 0;
        if !removed.is_empty() {
            wraps += self.tree_remove(cx, g, &removed);
        }
        let (paths, grow_wraps) = if joined.is_empty() {
            (Vec::new(), 0)
        } else {
            self.tree_add(cx, g, &joined)
        };
        wraps += grow_wraps;
        {
            let grp = self.groups.get_mut(&g).expect("그룹");
            grp.t = grp.tree.as_ref().map_or(NONE, |t| t.nodes[1]);
        }
        if !removed.is_empty() {
            self.new_r(cx, g);
        }
        let r = self.groups[&g].r;
        let mut joined_paths = Vec::new();
        for (d, keys) in &paths {
            if g == CREW {
                for k in &keys[1..] {
                    cx.kn.seal(*d, *k);
                }
                cx.kn.seal(*d, r);
                joined_paths.push((*d, keys.len() - 1));
            } else {
                // 비공개 그룹: 잎(크루 잎에서 나옴)으로 경로와 R을 감쌉니다(X-Wing 없음).
                for k in &keys[1..] {
                    cx.kn.wrap(keys[0], *k);
                }
                cx.kn.wrap(keys[0], r);
                wraps += keys.len();
            }
        }
        let ids = (removed.len() + joined.len()) * KU_DEVICE;
        let var = wraps * m.wrap + if removed.is_empty() { 0 } else { R_CT };
        let what = if removed.is_empty() {
            "KeyUpdate(전진)"
        } else {
            "KeyUpdate(회전)"
        };
        self.publish(&mut c, cx, what, KU_FIXED + ids, var, n);
        if g == CREW && !joined_paths.is_empty() {
            self.inbox(cx, &mut c, &joined_paths);
        }
        self.settle(cx, g);
        c
    }

    fn flush(&mut self, cx: &mut Cx<'_>) -> Vec<OpCost> {
        cx.kn.begin();
        cx.kn.set_issuer(Some(self.committer));
        if self.need_full {
            let mut out = self.full_rekey(cx, CostClass::FullRekey);
            // 원인(어떤 제거가 표 보유 기기를 뺐는지)은 전체 재키로 셉니다.
            for c in &mut out {
                c.class = CostClass::FullRekey;
            }
            return out;
        }
        let groups = self.pending.take();
        groups
            .iter()
            .map(|(g, pg)| self.flush_group(cx, *g, pg))
            .filter(|c| c.events > 0)
            .collect()
    }

    /// 뿌리와 R만 새로(시간 회전, 새 확정 기기의 첫 회전).
    fn refresh(&mut self, cx: &mut Cx<'_>, g: Group, class: CostClass) -> OpCost {
        let mut c = OpCost::new(class, g);
        let grp = self.groups.get_mut(&g).expect("그룹");
        if grp.members.is_empty() {
            return c;
        }
        let n = grp.members.len();
        let Some(tree) = grp.tree.as_mut() else {
            self.direct_rotate(cx, g, &mut c, 0);
            self.settle(cx, g);
            return c;
        };
        let k = cx.kn.key(Scope::Group(g), true);
        let mut wraps = 0;
        for kid in [2, 3] {
            if tree.present(kid) {
                cx.kn.wrap(tree.nodes[kid], k);
                wraps += 1;
            }
        }
        tree.nodes[1] = k;
        grp.t = k;
        self.new_r(cx, g);
        let var = wraps * cx.model.wrap + R_CT;
        self.publish(&mut c, cx, "KeyUpdate(뿌리 새로)", KU_FIXED, var, n);
        self.settle(cx, g);
        c
    }

    /// 살아 있는 확정 기기가 넘겨줌: leaf_master와 노드 키 스냅숏을 직접 요청으로 봉인해 넘기고, 새 기기가 회전.
    /// 잎은 넘기지 않습니다(크루 잎은 leaf_master에서, 비공개 그룹 잎은 크루 잎에서 계산). 봉인으로만 준 잎만 넘깁니다.
    fn handover(&mut self, cx: &mut Cx<'_>, new: Dev) -> Vec<OpCost> {
        let mut c = OpCost::new(CostClass::Committer, CREW);
        let mut keys = 0usize;
        if let Some(lm) = self.leaf_master {
            cx.kn.seal(new, lm);
            keys += 1;
        }
        for grp in self.groups.values() {
            match &grp.tree {
                Some(t) => {
                    let leaves = 1usize << t.depth;
                    for (i, &k) in t.nodes.iter().enumerate() {
                        if k != NONE && (i < leaves || self.lone_leaves.contains(&k)) {
                            cx.kn.seal(new, k);
                            keys += 1;
                        }
                    }
                }
                None if grp.t != NONE => {
                    cx.kn.seal(new, grp.t);
                    keys += 1;
                }
                None => {}
            }
            if grp.r != NONE {
                cx.kn.seal(new, grp.r);
                keys += 1;
            }
        }
        c.direct = (cx.model.seal(32) + keys * 36) as u64;
        c.xwing = 1;
        self.stats.handover_bytes = self.stats.handover_bytes.max(c.direct);
        self.committer = new;
        self.holders.insert(new);
        cx.kn.set_issuer(Some(new));
        cx.kn.begin();
        let mut out = vec![c];
        let groups: Vec<Group> = self.groups.keys().copied().collect();
        for g in groups {
            let r = self.refresh(cx, g, CostClass::Committer);
            if r.events > 0 {
                out.push(r);
            }
        }
        out
    }

    /// 이 변화로 표 보유 기기가 빠지는지: 크루에서 빠짐·강등·예비 기기 교체(`demoted`), 또는 어느 키 그룹의 자격을
    /// 잃음. 표 보유 기기는 leaf_master로 모든 잎을 계산하므로 한 그룹이라도 자격을 잃으면 전체 재키가 필요합니다.
    fn lost_holder(&self, d: &Delta) -> bool {
        #[cfg(test)]
        if self.mutant == Mutant::NoHolderRekey {
            return false;
        }
        d.demoted.iter().any(|x| self.holders.contains(x))
            || d.removed.iter().any(|(_, x)| self.holders.contains(x))
    }

    fn checkpoint_tick(&mut self, now: u64, m: &super::CostModel) {
        self.events_since_checkpoint += 1;
        if self.events_since_checkpoint >= m.checkpoint_events
            || now >= self.last_checkpoint + m.checkpoint_ms
        {
            self.events_since_checkpoint = 0;
            self.r_since_checkpoint = 0;
            self.last_checkpoint = now;
        }
    }
}

impl KeyScheme for KeyTreeModel {
    fn name(&self) -> &'static str {
        "tree"
    }

    fn init(&mut self, cx: &mut Cx<'_>) -> Vec<OpCost> {
        self.committer = cx.world.committer;
        self.holders.insert(self.committer);
        for g in 0..cx.world.groups.len() as Group {
            self.groups.insert(
                g,
                KGroup {
                    members: cx.world.group(g).clone(),
                    tree: None,
                    t: NONE,
                    r: NONE,
                    last_r: 0,
                    joins_since_r: 0,
                },
            );
        }
        let mut out = self.full_rekey(cx, CostClass::Setup);
        self.stats.full_rekeys = 0;
        self.stats.blob_per_user.clear();
        self.stats.full_device_download.clear();
        for c in &mut out {
            c.class = CostClass::Setup;
        }
        out
    }

    fn apply(&mut self, cx: &mut Cx<'_>, ev: &Event, d: &Delta) -> Vec<OpCost> {
        let mut out = Vec::new();
        self.checkpoint_tick(cx.now, cx.model);
        cx.kn.set_issuer(Some(self.committer));
        let lost_holder = self.lost_holder(d);
        let mut full_done = false;
        if let Some(b) = d.backup {
            self.backup = b;
            if let (Some(b), Some(lm)) = (b, self.leaf_master) {
                cx.kn.seal(b, lm);
                let mut c = OpCost::new(CostClass::Backup, CREW);
                c.xwing = 1;
                c.direct = cx.model.seal(32) as u64;
                out.push(c);
                self.holders.insert(b);
            }
        }
        if let Some((_, new, how)) = d.committer {
            match how {
                Handover::Voluntary => {
                    if !self.pending.is_empty() {
                        out.extend(self.flush(cx));
                    }
                    out.extend(self.handover(cx, new));
                }
                Handover::Lost | Handover::Repair => {
                    // 넘겨받지 못함: 새 확정 기기가 전체 재키합니다(기다리던 변화도 덮음).
                    self.committer = new;
                    out.extend(self.full_rekey(cx, CostClass::FullRekey));
                    full_done = true;
                }
            }
        }
        let has_change = !d.added.is_empty() || !d.removed.is_empty();
        if lost_holder && !full_done {
            if has_change {
                // 표 보유 기기가 빠짐: 다음 회전(묶음 창 끝)은 전체 재키여야 합니다.
                self.need_full = true;
            } else {
                out.extend(self.full_rekey(cx, CostClass::FullRekey));
                full_done = true;
            }
        }
        if !full_done {
            self.pending.push(d, &ev.op, ev.at_ms, cx.model, true);
        }
        out
    }

    fn advance(&mut self, cx: &mut Cx<'_>) -> Vec<OpCost> {
        let mut out = Vec::new();
        if self.pending.due(cx.now).is_some() || (self.need_full && self.pending.is_empty()) {
            out.extend(self.flush(cx));
        }
        if cx.now >= self.last_full + cx.model.full_rekey_ms {
            if !self.pending.is_empty() {
                out.extend(self.flush(cx));
            }
            cx.kn.begin();
            out.extend(self.full_rekey(cx, CostClass::FullRekey));
        }
        let due: Vec<Group> = self
            .groups
            .iter()
            .filter(|(_, x)| {
                !x.members.is_empty()
                    && (cx.now >= x.last_r + cx.model.time_rotation_ms
                        || x.joins_since_r >= cx.model.time_rotation_joins)
            })
            .map(|(g, _)| *g)
            .collect();
        if !due.is_empty() {
            if !self.pending.is_empty() {
                out.extend(self.flush(cx));
            }
            cx.kn.begin();
            cx.kn.set_issuer(Some(self.committer));
            for g in due {
                let c = self.refresh(cx, g, CostClass::TimeRotation);
                if c.events > 0 {
                    out.push(c);
                }
            }
        }
        out
    }

    fn resync(&mut self, cx: &mut Cx<'_>, d: &Delta) -> Vec<OpCost> {
        let mut out = Vec::new();
        if !self.pending.is_empty() {
            out.extend(self.flush(cx));
        }
        if self.lost_holder(d) {
            self.need_full = true;
        }
        self.pending
            .push(d, &Op::PartitionEnd, cx.now, cx.model, true);
        out.extend(self.flush(cx));
        out
    }

    fn finish(&mut self, cx: &mut Cx<'_>) -> Vec<OpCost> {
        if self.pending.is_empty() && !self.need_full {
            return Vec::new();
        }
        self.flush(cx)
    }

    fn boxed_clone(&self) -> Box<dyn KeyScheme + Send> {
        Box::new(self.clone())
    }

    fn notes(&self) -> Vec<(String, String)> {
        let s = &self.stats;
        let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
        let mut v = vec![
            ("전체 재키 횟수".into(), s.full_rekeys.to_string()),
            (
                "전체 재키 블롭/사용자".into(),
                format!("{:.0}B", mean(&s.blob_per_user)),
            ),
            (
                "전체 재키 기기당 받기".into(),
                format!("{:.0}B", mean(&s.full_device_download)),
            ),
            ("최대 트리 깊이".into(), s.max_depth.to_string()),
            ("인코딩 바뀜".into(), s.mode_switches.to_string()),
            ("나눠 낸 이벤트".into(), s.split_events.to_string()),
            (
                "표 넘겨주기(최대)".into(),
                format!("{:.1}MB", s.handover_bytes as f64 / 1e6),
            ),
        ];
        let modes: Vec<String> = self
            .groups
            .iter()
            .map(|(g, x)| {
                format!(
                    "{g}:{}{}",
                    x.members.len(),
                    if x.tree.is_some() { "T" } else { "D" }
                )
            })
            .collect();
        v.push(("그룹(끝, 기기·인코딩)".into(), modes.join(" ")));
        v
    }
}
