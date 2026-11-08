//! 누가 어떤 키를 얻을 수 있는지 기호로 따라가는 모형과 불변식 검사.
//!
//! 키 방식은 키를 만들고(`key`), 한 키에서 다른 키를 계산하게 하거나(`derive`, KDF), 한 키로 다른 키를 감싸거나
//! (`wrap`), 기기 상자 키로 봉인합니다(`seal`). 감싼 것은 모두 공개된 것으로 봅니다(제거된 기기도 스트림을 계속
//! 받을 수 있다고 가정). 검사기는 방식의 내부를 모르고 이 그래프와, 기록에서 나온 자격 일정(누가 언제 어느 그룹의
//! 멤버였는지)과 표 보유 일정(확정 기기·예비 기기)만으로 판정합니다.
//!
//! 기기 d가 키 k를 얻는 시각 acq = 봉인 시각, 또는 감싸기·KDF 경로를 따라 max(앞 키를 얻은 시각, 그 간선이
//! 생긴 시각)의 최소. 판정:
//! - 얻는 시각에 d가 k의 범위에 들지 않으면 I2 위반. 그룹 키는 그 그룹 자격만 봅니다(표 보유 기기도 자격이 없는
//!   그룹의 키를 얻으면 위반). 잎 키는 자기 것이거나 표 보유 기기(leaf_master로 모든 잎을 계산)면, leaf_master는
//!   표 보유 기기면 됩니다. 키를 만든 기기(확정 기기)는 자기가 만든 키를 압니다(설계상 확정 기기는 모든 키를 씀).
//!   그 키에서 나중에 남이 만든 키는 다시 자격으로 판정합니다.
//! - 그룹 키가 정착할 때(`settle`) 자격을 잃은 기기가 그 키를 이미 가지고 있어도 I2 위반입니다(낡은 키: 제거 뒤
//!   회전하지 않고 옛 키를 계속 쓰는 방식). 자격을 다시 얻은 뒤 받은 것은 셈하지 않습니다.
//! - 기록 비공개 키(T 사슬·트리 노드)는 만들어진 시각에도 d가 자격이 있었어야 합니다. 아니면 I3 위반(가입 전 키).
//! - 그룹 키가 정착할 때(`settle`, 묶음 처리가 끝난 때) 자격 있는 모든 기기가 그 키를 얻을 수 있어야 합니다(I1).
//! - 이벤트 본문은 레인 한도 안이어야 합니다(I4: 일반 256KiB, 코드 80..=99는 960KiB).
//!
//! 갈라짐 동안 한쪽이 낸 키는 그쪽이 본 상태(보기 A·B)로 판정합니다. 상대쪽에서 일어난 제거를 모르고 낸 키를 그
//! 기기가 얻는 것은 어떤 방식도 피할 수 없기 때문입니다. 합친 뒤에는 모두 하나의 보기입니다.

use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap, HashMap};

use serde::Serialize;
use yangpa_core::event::{MAX_BODY_LEN, MAX_KEY_UPDATE_BODY_LEN};

use crate::scale::trace::{Dev, Group};

pub type KeyId = u32;
pub type Tick = u32;

/// 키를 낸 쪽의 상태 보기.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum View {
    Main,
    A,
    B,
}

impl View {
    pub fn bit(self) -> u8 {
        match self {
            View::Main => 1,
            View::A => 2,
            View::B => 4,
        }
    }

    fn index(self) -> usize {
        match self {
            View::Main => 0,
            View::A => 1,
            View::B => 2,
        }
    }
}

pub const ALL_VIEWS: u8 = 7;

/// 키를 가져도 되는 범위.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// 그 그룹의 자격 있는 기기.
    Group(Group),
    /// 그 기기만(잎 키).
    Device(Dev),
    /// 표 보유 기기만(leaf_master).
    Table,
}

/// 이벤트 레인(본문 한도).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Lane {
    /// 일반 이벤트(256KiB).
    Normal,
    /// 코드 80..=99(키 합의 레인: `KeyUpdate`, 옛 모형의 MLS 커밋·웰컴, 960KiB).
    KeyAgreement,
}

impl Lane {
    pub fn cap(self) -> usize {
        match self {
            Lane::Normal => MAX_BODY_LEN,
            Lane::KeyAgreement => MAX_KEY_UPDATE_BODY_LEN,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct KeyMeta {
    scope: Scope,
    view: u8,
    history_private: bool,
    created: Tick,
    issuer: Dev,
}

const NO_DEV: Dev = Dev::MAX;

#[derive(Clone, Copy, Debug)]
struct Change {
    tick: Tick,
    on: bool,
    mask: u8,
}

#[derive(Clone, Copy, Debug)]
struct Settle {
    tick: Tick,
    view: u8,
    key: KeyId,
}

/// 검사 결과.
#[derive(Clone, Debug, Default, Serialize)]
pub struct InvariantReport {
    /// 기호 모형을 따라갔는지(끄면 I4만).
    pub tracked: bool,
    pub devices: usize,
    pub checked: usize,
    pub keys: usize,
    pub edges: usize,
    pub seals: usize,
    /// 위반 (기기, 키) 쌍 수와 위반한 기기 수.
    pub i1: u64,
    pub i1_devices: u64,
    pub i2: u64,
    pub i2_devices: u64,
    /// I2 가운데 낡은 키(자격을 잃은 뒤 정착한 키를 이미 가짐).
    pub i2_stale: u64,
    pub i3: u64,
    pub i3_devices: u64,
    /// 레인 한도를 넘은 이벤트 수와 그중 가장 큰 본문.
    pub i4: u64,
    pub i4_max_body: usize,
    pub examples: Vec<String>,
}

impl InvariantReport {
    pub fn violations(&self) -> u64 {
        self.i1 + self.i2 + self.i3 + self.i4
    }
}

/// 검사 범위.
#[derive(Clone, Copy, Debug)]
pub struct CheckOpts {
    /// 기기가 이보다 적으면 모두 검사합니다.
    pub full_below: usize,
    /// 그보다 많으면 '변화가 있던 기기'(제거·가입·권한 바뀜·표 보유) 전부에 더해 나머지에서 이만큼 고릅니다.
    pub sample: usize,
}

impl Default for CheckOpts {
    fn default() -> Self {
        CheckOpts {
            full_below: 8_192,
            sample: 2_000,
        }
    }
}

/// 기호 모형.
pub struct Knowledge {
    track: bool,
    now: Tick,
    view: View,
    mask: u8,
    issuer: Dev,
    n_keys: u32,
    keys: Vec<KeyMeta>,
    edges: Vec<(KeyId, KeyId, Tick)>,
    seals: Vec<(Dev, KeyId, Tick)>,
    elig: HashMap<(Dev, Group), Vec<Change>>,
    table: HashMap<Dev, Vec<Change>>,
    table_now: [BTreeSet<Dev>; 3],
    settles: Vec<(Group, Settle)>,
    over_cap: Vec<(Tick, &'static str, usize, usize)>,
    over_cap_count: u64,
    max_body: usize,
}

impl Knowledge {
    /// `track`이 거짓이면 키 그래프를 저장하지 않고(큰 크루의 지금 MLS처럼 봉인이 기기 수에 비례할 때) I4만 셉니다.
    pub fn new(track: bool) -> Self {
        Knowledge {
            track,
            now: 0,
            view: View::Main,
            mask: ALL_VIEWS,
            issuer: NO_DEV,
            n_keys: 0,
            keys: Vec::new(),
            edges: Vec::new(),
            seals: Vec::new(),
            elig: HashMap::new(),
            table: HashMap::new(),
            table_now: Default::default(),
            settles: Vec::new(),
            over_cap: Vec::new(),
            over_cap_count: 0,
            max_body: 0,
        }
    }

    pub fn tracked(&self) -> bool {
        self.track
    }

    /// 논리 시각을 하나 넘깁니다(상태 변화 하나, 키 이벤트 묶음 하나마다).
    pub fn begin(&mut self) -> Tick {
        self.now += 1;
        self.now
    }

    pub fn now(&self) -> Tick {
        self.now
    }

    /// 지금 키를 내는 쪽의 보기와, 지금 기록하는 자격 변화가 어느 보기에 보이는지.
    pub fn set_view(&mut self, view: View, mask: u8) {
        self.view = view;
        self.mask = mask;
    }

    pub fn view(&self) -> View {
        self.view
    }

    /// 앞으로 만드는 키를 아는 기기(키를 뽑은 확정 기기).
    pub fn set_issuer(&mut self, dev: Option<Dev>) {
        self.issuer = dev.unwrap_or(NO_DEV);
    }

    pub fn set_eligible(&mut self, dev: Dev, group: Group, on: bool) {
        if !self.track {
            return;
        }
        self.elig.entry((dev, group)).or_default().push(Change {
            tick: self.now,
            on,
            mask: self.mask,
        });
    }

    /// 표 보유(leaf_master를 가질 수 있는) 기기 지정·해제. 기록이 정합니다(확정 기기·예비 기기, 제거·강등).
    pub fn set_table(&mut self, dev: Dev, on: bool) {
        for v in [View::Main, View::A, View::B] {
            if self.mask & v.bit() != 0 {
                if on {
                    self.table_now[v.index()].insert(dev);
                } else {
                    self.table_now[v.index()].remove(&dev);
                }
            }
        }
        if !self.track {
            return;
        }
        self.table.entry(dev).or_default().push(Change {
            tick: self.now,
            on,
            mask: self.mask,
        });
    }

    /// 지금 보기에서 표 보유 기기인지.
    pub fn is_table(&self, dev: Dev) -> bool {
        self.table_now[self.view.index()].contains(&dev)
    }

    /// 방식이 전체 재키를 마쳤다고 알립니다: 표 보유 기기를 `keep`만 남깁니다. 검사를 엄격하게만 바꿉니다
    /// (실제로 새 leaf_master를 쓰지 않았다면 빠진 기기가 새 키를 얻어 I2로 드러남).
    pub fn reset_table(&mut self, keep: &[Dev]) {
        let views: Vec<View> = [View::Main, View::A, View::B]
            .into_iter()
            .filter(|v| self.mask & v.bit() != 0)
            .collect();
        for v in views {
            let drop: Vec<Dev> = self.table_now[v.index()]
                .iter()
                .filter(|d| !keep.contains(d))
                .copied()
                .collect();
            for d in drop {
                self.table_now[v.index()].remove(&d);
                if self.track {
                    self.table.entry(d).or_default().push(Change {
                        tick: self.now,
                        on: false,
                        mask: v.bit(),
                    });
                }
            }
        }
    }

    /// 갈라짐 합치기: 지금 시각부터 A·B 보기도 합친 상태(Main)와 같게 맞춥니다. 진 쪽이 낸 키를 합친 뒤에 받는
    /// 기기는 합친 상태로 판정해야 하기 때문입니다.
    pub fn merge_views(&mut self) {
        let now = self.now;
        let converge = |c: &mut Vec<Change>| {
            let main = Self::at(Some(c), View::Main.bit(), now);
            for v in [View::A, View::B] {
                if Self::at(Some(c), v.bit(), now) != main {
                    c.push(Change {
                        tick: now,
                        on: main,
                        mask: v.bit(),
                    });
                }
            }
        };
        self.elig.values_mut().for_each(&converge);
        self.table.values_mut().for_each(&converge);
        let main = self.table_now[View::Main.index()].clone();
        self.table_now[View::A.index()] = main.clone();
        self.table_now[View::B.index()] = main;
    }

    /// 새 키(지금 시각·보기, 지금 확정 기기가 앎).
    pub fn key(&mut self, scope: Scope, history_private: bool) -> KeyId {
        let id = self.n_keys;
        self.n_keys += 1;
        if self.track {
            self.keys.push(KeyMeta {
                scope,
                view: self.view.bit(),
                history_private,
                created: self.now,
                issuer: self.issuer,
            });
        }
        id
    }

    /// `from`을 아는 기기가 계산할 수 있는 새 키(KDF).
    pub fn derive(&mut self, from: KeyId, scope: Scope, history_private: bool) -> KeyId {
        let k = self.key(scope, history_private);
        self.edge(from, k);
        k
    }

    /// `under`로 `key`를 감싼 것을 냄(감싼 것은 공개).
    pub fn wrap(&mut self, under: KeyId, key: KeyId) {
        self.edge(under, key);
    }

    fn edge(&mut self, from: KeyId, to: KeyId) {
        if self.track {
            self.edges.push((from, to, self.now));
        }
    }

    /// 기기 상자 키(X-Wing)로 봉인해 냄.
    pub fn seal(&mut self, dev: Dev, key: KeyId) {
        if self.track {
            self.seals.push((dev, key, self.now));
        }
    }

    /// 그룹의 지금 키가 정해짐: 이 시각에 자격 있는 모든 기기가 얻을 수 있어야 합니다(I1).
    pub fn settle(&mut self, group: Group, key: KeyId) {
        if self.track {
            self.settles.push((
                group,
                Settle {
                    tick: self.now,
                    view: self.view.bit(),
                    key,
                },
            ));
        }
    }

    /// 이벤트 본문 하나(I4).
    pub fn body(&mut self, what: &'static str, lane: Lane, len: usize) {
        self.max_body = self.max_body.max(len);
        if len > lane.cap() {
            self.over_cap_count += 1;
            if self.over_cap.len() < 8 {
                self.over_cap.push((self.now, what, len, lane.cap()));
            }
        }
    }

    pub fn max_body(&self) -> usize {
        self.max_body
    }

    fn at(changes: Option<&Vec<Change>>, view: u8, t: Tick) -> bool {
        let Some(c) = changes else { return false };
        let end = c.partition_point(|x| x.tick <= t);
        c[..end]
            .iter()
            .rev()
            .find(|x| x.mask & view != 0)
            .is_some_and(|x| x.on)
    }

    /// 보기 `view`에서 (기기, 그룹)이 자격 있던 구간들 [시작, 끝).
    fn intervals(&self, view: u8, dev: Dev, group: Group) -> Vec<(Tick, Tick)> {
        let mut out = Vec::new();
        let mut start = None;
        for c in self.elig.get(&(dev, group)).into_iter().flatten() {
            if c.mask & view == 0 {
                continue;
            }
            match (c.on, start) {
                (true, None) => start = Some(c.tick),
                (false, Some(s)) => {
                    out.push((s, c.tick));
                    start = None;
                }
                _ => {}
            }
        }
        if let Some(s) = start {
            out.push((s, Tick::MAX));
        }
        out
    }

    /// 불변식 검사. 기기마다 봉인·발행에서 시작해 얻을 수 있는 키를 얻는 시각 순서로 따라갑니다.
    pub fn check(&self, opts: CheckOpts) -> InvariantReport {
        let mut rep = InvariantReport {
            tracked: self.track,
            keys: self.n_keys as usize,
            edges: self.edges.len(),
            seals: self.seals.len(),
            i4: self.over_cap_count,
            i4_max_body: self.max_body,
            ..Default::default()
        };
        for (t, what, len, cap) in &self.over_cap {
            rep.examples
                .push(format!("I4: 시각 {t} {what} 본문 {len}B > 한도 {cap}B"));
        }
        if !self.track {
            return rep;
        }
        // 정방향 인접 목록(CSR).
        let n = self.keys.len();
        let mut start = vec![0u32; n + 1];
        for &(f, _, _) in &self.edges {
            start[f as usize + 1] += 1;
        }
        for i in 0..n {
            start[i + 1] += start[i];
        }
        let mut fill = start.clone();
        let mut adj = vec![(0u32, 0u32); self.edges.len()];
        for &(f, to, t) in &self.edges {
            let i = &mut fill[f as usize];
            adj[*i as usize] = (to, t);
            *i += 1;
        }
        let mut seeds: HashMap<Dev, Vec<(Tick, KeyId)>> = HashMap::new();
        for &(d, k, t) in &self.seals {
            seeds.entry(d).or_default().push((t, k));
        }
        for (k, m) in self.keys.iter().enumerate() {
            if m.issuer != NO_DEV {
                seeds
                    .entry(m.issuer)
                    .or_default()
                    .push((m.created, k as KeyId));
            }
        }
        let mut settles: HashMap<Group, Vec<Settle>> = HashMap::new();
        for (g, s) in &self.settles {
            settles.entry(*g).or_default().push(*s);
        }

        // 검사할 기기.
        let mut devices: BTreeSet<Dev> = self.elig.keys().map(|(d, _)| *d).collect();
        devices.extend(seeds.keys().copied());
        rep.devices = devices.len();
        let chosen: Vec<Dev> = if devices.len() < opts.full_below {
            devices.into_iter().collect()
        } else {
            // 기기별 변화 여부를 한 번에 모읍니다(위 클로저를 기기마다 돌리면 느림).
            let mut moved: BTreeSet<Dev> = self.table.keys().copied().collect();
            for ((d, _), c) in &self.elig {
                if c.iter().any(|ch| !ch.on || ch.tick > 1) {
                    moved.insert(*d);
                }
            }
            let rest: Vec<Dev> = devices
                .iter()
                .filter(|d| !moved.contains(d))
                .copied()
                .collect();
            let step = (rest.len() / opts.sample.max(1)).max(1);
            moved
                .into_iter()
                .chain(rest.into_iter().step_by(step))
                .collect()
        };
        rep.checked = chosen.len();

        let groups: Vec<Group> = {
            let mut g: BTreeSet<Group> = self.elig.keys().map(|(_, g)| *g).collect();
            g.extend(settles.keys().copied());
            g.into_iter().collect()
        };
        let threads = std::thread::available_parallelism()
            .map_or(1, |p| p.get())
            .min(chosen.len().max(1));
        let chunk = chosen.len().div_ceil(threads).max(1);
        let ctx = CheckCx {
            k: self,
            start: &start,
            adj: &adj,
            seeds: &seeds,
            settles: &settles,
            groups: &groups,
            max_group: groups.last().copied().unwrap_or(0),
        };
        let parts: Vec<Partial> = std::thread::scope(|s| {
            let handles: Vec<_> = chosen
                .chunks(chunk)
                .map(|devs| {
                    let ctx = &ctx;
                    s.spawn(move || {
                        let mut w = Walker::new(n);
                        let mut p = Partial::default();
                        for &d in devs {
                            w.check(ctx, d, &mut p);
                        }
                        p
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("검사 스레드"))
                .collect()
        });
        let mut examples = Vec::new();
        for p in parts {
            rep.i1 += p.i1;
            rep.i1_devices += p.i1_devices;
            rep.i2 += p.i2;
            rep.i2_devices += p.i2_devices;
            rep.i2_stale += p.i2_stale;
            rep.i3 += p.i3;
            rep.i3_devices += p.i3_devices;
            examples.extend(p.examples);
        }
        examples.sort();
        examples.truncate(12);
        rep.examples.extend(examples);
        rep
    }
}

struct CheckCx<'a> {
    k: &'a Knowledge,
    start: &'a [u32],
    adj: &'a [(KeyId, Tick)],
    seeds: &'a HashMap<Dev, Vec<(Tick, KeyId)>>,
    settles: &'a HashMap<Group, Vec<Settle>>,
    groups: &'a [Group],
    max_group: Group,
}

#[derive(Default)]
struct Partial {
    i1: u64,
    i1_devices: u64,
    i2: u64,
    i2_devices: u64,
    i2_stale: u64,
    i3: u64,
    i3_devices: u64,
    examples: Vec<String>,
}

/// 기기 하나씩 따라가는 작업 공간(도장으로 초기화를 아낌).
struct Walker {
    stamp: Vec<u32>,
    done: Vec<u32>,
    /// 이 기기에서 이미 I2로 센 키(낡은 키 검사에서 두 번 세지 않게).
    bad: Vec<u32>,
    acq: Vec<Tick>,
    cur: u32,
    heap: BinaryHeap<Reverse<(Tick, KeyId)>>,
}

impl Walker {
    fn new(n: usize) -> Self {
        Walker {
            stamp: vec![0; n],
            done: vec![0; n],
            bad: vec![0; n],
            acq: vec![0; n],
            cur: 0,
            heap: BinaryHeap::new(),
        }
    }

    fn reach(&mut self, k: KeyId, t: Tick) {
        let i = k as usize;
        if self.stamp[i] != self.cur || t < self.acq[i] {
            self.stamp[i] = self.cur;
            self.acq[i] = t;
            self.heap.push(Reverse((t, k)));
        }
    }

    fn acquired(&self, k: KeyId) -> Option<Tick> {
        (self.stamp[k as usize] == self.cur).then(|| self.acq[k as usize])
    }

    fn check(&mut self, cx: &CheckCx<'_>, d: Dev, p: &mut Partial) {
        self.cur += 1;
        let (mut v1, mut v2, mut v3, mut stale) = (0u64, 0u64, 0u64, 0u64);
        let kn = cx.k;
        // 이 기기의 일정은 한 번만 찾습니다(키마다 해시 조회하면 느림).
        let table = kn.table.get(&d);
        let elig: Vec<Option<&Vec<Change>>> =
            (0..=cx.max_group).map(|g| kn.elig.get(&(d, g))).collect();
        let eligible = |view: u8, g: Group, t: Tick| {
            Knowledge::at(elig.get(g as usize).copied().flatten(), view, t)
        };
        for &(t, k) in cx.seeds.get(&d).map_or(&[][..], |v| v.as_slice()) {
            self.reach(k, t);
        }
        while let Some(Reverse((a, k))) = self.heap.pop() {
            let i = k as usize;
            if self.done[i] == self.cur || self.acq[i] < a {
                continue;
            }
            self.done[i] = self.cur;
            let m = &kn.keys[i];
            // 표 보유 기기는 leaf_master와 거기서 나오는 잎만 더 가질 수 있습니다. 그룹 키는 그 그룹 자격으로만
            // 판정합니다(leaf_master로 비공개 그룹 잎·경로를 계산해 자격 없는 그룹을 읽으면 위반).
            let own = m.issuer == d;
            let ok = own
                || match m.scope {
                    Scope::Group(g) => eligible(m.view, g, a),
                    Scope::Device(x) => x == d || Knowledge::at(table, m.view, a),
                    Scope::Table => Knowledge::at(table, m.view, a),
                };
            if !ok {
                v2 += 1;
                self.bad[i] = self.cur;
                if p.examples.len() < 4 {
                    p.examples.push(format!(
                        "I2: 기기 {d}가 자격 없이 시각 {a}에 키 {k}({:?}, 만든 시각 {})를 얻음",
                        m.scope, m.created
                    ));
                }
            } else if let (false, true, Scope::Group(g)) = (own, m.history_private, m.scope)
                && !eligible(m.view, g, m.created)
            {
                v3 += 1;
                if p.examples.len() < 4 {
                    p.examples.push(format!(
                        "I3: 기기 {d}가 가입 전(시각 {}) 기록 비공개 키 {k}(그룹 {g})를 얻음",
                        m.created
                    ));
                }
            }
            for &(to, t) in &cx.adj[cx.start[i] as usize..cx.start[i + 1] as usize] {
                self.reach(to, a.max(t));
            }
        }
        // I1: 자격 있던 구간 안에 정착한 그룹 키를 모두 그때까지 얻었는지. 낡은 키(I2): 자격을 잃은 뒤 다시 얻기
        // 전에 정착한 키를 그 정착 시각까지 이미 가졌는지(제거 뒤 회전하지 않으면 옛 키가 다시 정착함).
        for &g in cx.groups {
            let Some(list) = cx.settles.get(&g) else {
                continue;
            };
            for view in [1u8, 2, 4] {
                let iv = kn.intervals(view, d, g);
                for (s, e) in iv.iter().copied() {
                    let lo = list.partition_point(|x| x.tick < s);
                    let hi = list.partition_point(|x| x.tick < e);
                    for st in &list[lo..hi] {
                        if st.view != view {
                            continue;
                        }
                        if !self.acquired(st.key).is_some_and(|a| a <= st.tick) {
                            v1 += 1;
                            if p.examples.len() < 4 {
                                p.examples.push(format!(
                                    "I1: 기기 {d}가 시각 {}에 그룹 {g}의 지금 키 {}를 얻지 못함",
                                    st.tick, st.key
                                ));
                            }
                        }
                    }
                }
                // 자격 구간 사이(그리고 마지막 구간 뒤)의 빈틈.
                let gaps = iv
                    .iter()
                    .zip(iv.iter().skip(1).map(|x| x.0).chain([Tick::MAX]))
                    .map(|(x, next)| (x.1, next))
                    .filter(|(e, next)| e < next);
                for (e, next) in gaps {
                    let lo = list.partition_point(|x| x.tick < e);
                    let hi = list.partition_point(|x| x.tick < next);
                    for st in &list[lo..hi] {
                        if st.view != view || self.bad[st.key as usize] == self.cur {
                            continue;
                        }
                        if self.acquired(st.key).is_some_and(|a| a <= st.tick) {
                            self.bad[st.key as usize] = self.cur;
                            v2 += 1;
                            stale += 1;
                            if p.examples.len() < 4 {
                                p.examples.push(format!(
                                    "I2: 기기 {d}가 그룹 {g} 자격을 잃은 뒤(시각 {e}) 시각 {}에 정착한 키 {}를 이미 가짐(회전 없음)",
                                    st.tick, st.key
                                ));
                            }
                        }
                    }
                }
            }
        }
        p.i1 += v1;
        p.i2 += v2;
        p.i2_stale += stale;
        p.i3 += v3;
        p.i1_devices += u64::from(v1 > 0);
        p.i2_devices += u64::from(v2 > 0);
        p.i3_devices += u64::from(v3 > 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 제거 뒤 KDF로 전진한 키를 제거된 기기가 계산하면 I2, 가입자가 전 키를 받으면 I3, 정착한 키를 못 받으면 I1.
    #[test]
    fn checker_flags_each_invariant() {
        let mut k = Knowledge::new(true);
        k.begin();
        for d in [1, 2, 3] {
            k.set_eligible(d, 0, true);
        }
        let t0 = k.key(Scope::Group(0), true);
        for d in [1, 2, 3] {
            k.seal(d, t0);
        }
        k.settle(0, t0);
        k.begin();
        k.set_eligible(3, 0, false); // 3 제거
        k.begin();
        let t1 = k.derive(t0, Scope::Group(0), true); // 회전 전에 전진: 3이 계산함(I2)
        let old = k.key(Scope::Group(0), true);
        k.begin();
        k.set_eligible(4, 0, true);
        let t2 = k.derive(t1, Scope::Group(0), true); // 가입 전진(3도 계산함: I2)
        k.seal(4, t2);
        k.seal(4, old); // 가입자에게 가입 전 키(I3)
        k.settle(0, t2);
        k.begin();
        let t3 = k.key(Scope::Group(0), true); // 1에게만 봉인(2·4가 못 받음: I1)
        k.seal(1, t3);
        k.settle(0, t3);
        k.body("시험", Lane::Normal, MAX_BODY_LEN + 1);
        let r = k.check(CheckOpts::default());
        assert_eq!((r.i2, r.i2_devices), (2, 1), "{r:?}");
        assert_eq!(r.i3, 1, "{r:?}");
        assert_eq!((r.i1, r.i1_devices), (2, 2), "{r:?}");
        assert_eq!(r.i4, 1, "{r:?}");
    }

    /// 제거 뒤 회전하지 않고 옛 키를 다시 정착하면 제거된 기기가 그 키를 이미 가지므로 낡은 키 I2입니다. 회전한
    /// 키를 정착하면 위반이 없고, 자격을 다시 얻은 뒤 지금 키를 받는 것도 위반이 아닙니다.
    #[test]
    fn checker_flags_stale_key_after_removal() {
        let run = |rotate: bool| {
            let mut k = Knowledge::new(true);
            k.begin();
            for d in [1, 2, 3] {
                k.set_eligible(d, 0, true);
            }
            let t0 = k.key(Scope::Group(0), true);
            let r0 = k.key(Scope::Group(0), false);
            for d in [1, 2, 3] {
                k.seal(d, t0);
            }
            k.wrap(t0, r0);
            k.settle(0, t0);
            k.settle(0, r0);
            k.begin();
            k.set_eligible(3, 0, false);
            k.begin();
            let (t, r) = if rotate {
                let t1 = k.key(Scope::Group(0), true);
                let r1 = k.key(Scope::Group(0), false);
                for d in [1, 2] {
                    k.seal(d, t1);
                }
                k.wrap(t1, r1);
                (t1, r1)
            } else {
                (t0, r0)
            };
            k.settle(0, t);
            k.settle(0, r);
            // 3이 다시 들어와 지금 R(기록 공개)을 받음.
            k.begin();
            k.set_eligible(3, 0, true);
            let t2 = k.derive(t, Scope::Group(0), true);
            k.seal(3, t2);
            k.seal(3, r);
            k.settle(0, t2);
            k.settle(0, r);
            k.check(CheckOpts::default())
        };
        let stale = run(false);
        assert_eq!(
            (stale.i2, stale.i2_stale, stale.i2_devices),
            (2, 2, 1),
            "{stale:?}"
        );
        let ok = run(true);
        assert_eq!(ok.violations(), 0, "{ok:?}");
    }

    /// 표 보유 기기는 leaf_master와 잎을 가져도 되지만, 거기서 계산한 그룹 키는 그 그룹 자격으로 판정합니다.
    #[test]
    fn table_holder_is_judged_by_group_eligibility() {
        let mut k = Knowledge::new(true);
        k.begin();
        for d in [1, 2] {
            k.set_eligible(d, 0, true);
        }
        k.set_eligible(1, 1, true);
        k.set_table(2, true);
        let lm = k.key(Scope::Table, true);
        k.seal(2, lm);
        let leaf = k.derive(lm, Scope::Device(1), true);
        let private = k.derive(leaf, Scope::Device(1), true);
        k.seal(1, private);
        let node = k.key(Scope::Group(1), true);
        k.wrap(private, node);
        k.settle(1, node);
        let r = k.check(CheckOpts::default());
        assert_eq!((r.i1, r.i3), (0, 0), "{r:?}");
        assert_eq!((r.i2, r.i2_devices), (1, 1), "{r:?}");
        assert!(r.examples.iter().any(|e| e.contains("기기 2")), "{r:?}");
    }
}
