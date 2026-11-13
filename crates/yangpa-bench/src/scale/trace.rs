//! 시드로 정해지는 멤버 변동 기록(키 관리 모형과 따라잡기 모형의 입력).
//!
//! 사용자는 기기 1~2대이고, 하루 가입·탈퇴(기본 사용자의 1%씩, 3만 명이면 300건), 기기 추가·폐기, 비공개 키 그룹
//! 열람 권한 바뀜이 무작위 시각에 일어납니다. 그 사이 정해진 시점(기록 길이의 비율)에 확정 기기 사건이 들어갑니다:
//! 예비 기기 지정·교체·폐기, 자발적 넘김, 관리자 확정 기기의 강등, 확정 기기 폐기, 지금 확정 기기가 아닌 표 보유
//! 기기(넘겨준 옛 확정 기기)의 강등·폐기, 두 확정 기기가 동시에 일하는 갈라짐과 합치기, 확정 기기 잃음, 밴
//! 물결(몇 번의 묶음), 소유권 이전. 키 모형은 이 기록을 [`World`]에 적용한 멤버십 변화([`Delta`])만 봅니다.
//!
//! 소유자와 관리자(ADMINISTRATOR)는 실제 권한 계산처럼 모든 채널을 보므로 모든 비공개 키 그룹에 듭니다. 강등되면
//! 역할로 받은 그룹만 남습니다. 소유권 이전 뒤 옛 소유자는 관리자 역할을 가진다고 봅니다.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use serde::Serialize;

/// 기기 번호(기록 안에서만 쓰는 값).
pub type Dev = u32;
/// 사용자 번호.
pub type User = u32;
/// 키 그룹 번호. 0은 크루 그룹, 1부터 비공개 키 그룹.
pub type Group = u16;
pub const CREW: Group = 0;

pub const SECOND_MS: u64 = 1_000;
pub const HOUR_MS: u64 = 3_600_000;
pub const DAY_MS: u64 = 86_400_000;

/// 기록을 만드는 조건.
#[derive(Clone, Debug, Serialize)]
pub struct Churn {
    /// 처음 기기 수. 사용자 수는 이 값 / 1.5(기기 1대·2대가 반반).
    pub devices: usize,
    pub days: f64,
    pub seed: u64,
    /// 사용자가 기기를 2대 가질 확률.
    pub second_device: f64,
    pub joins_per_day: f64,
    pub removals_per_day: f64,
    pub device_adds_per_day: f64,
    pub revocations_per_day: f64,
    /// 비공개 키 그룹 열람 권한이 바뀌는 사용자 수(하루).
    pub flips_per_day: f64,
    /// 비공개 키 그룹마다 볼 수 있는 사용자 비율.
    pub private_groups: Vec<f64>,
    /// 관리자 수(소유자 제외). 확정 기기 사건에 5명이 필요합니다.
    pub admins: usize,
    /// 밴 물결로 내보낼 사용자 수와 나눠 내는 이벤트 수(0이면 없음).
    pub mass_ban: usize,
    pub mass_ban_events: usize,
    /// 갈라짐 길이와 B쪽 기기 비율.
    pub partition_ms: u64,
    pub partition_share: f64,
    /// 확정 기기 사건(넘김·강등·폐기·갈라짐·잃음·소유권 이전)을 넣을지.
    pub committer_events: bool,
}

impl Churn {
    /// 기본 조건: 하루 가입·탈퇴 각 사용자의 1%, 기기 추가·폐기 각 0.1%, 권한 바뀜 0.2%, 비공개 키 그룹 7개
    /// (사용자의 40%·20%·10%·5%·2%·1%·0.5%), 밴 물결 min(1,500, 사용자/5)명을 세 번에 나눠.
    pub fn new(devices: usize, days: f64, seed: u64) -> Self {
        let users = users_for(devices) as f64;
        Churn {
            devices,
            days,
            seed,
            second_device: 0.5,
            joins_per_day: (users * 0.01).max(1.0),
            removals_per_day: (users * 0.01).max(1.0),
            device_adds_per_day: (users * 0.001).max(0.5),
            revocations_per_day: (users * 0.001).max(0.5),
            flips_per_day: (users * 0.002).max(0.5),
            private_groups: vec![0.40, 0.20, 0.10, 0.05, 0.02, 0.01, 0.005],
            admins: 8,
            mass_ban: (users as usize / 5).min(1_500),
            mass_ban_events: 3,
            partition_ms: HOUR_MS,
            partition_share: 0.1,
            committer_events: true,
        }
    }
}

/// 처음 사용자 수.
pub fn users_for(devices: usize) -> usize {
    ((devices as f64) / 1.5).round().max(8.0) as usize
}

/// 처음 멤버 한 명.
#[derive(Clone, Debug, Serialize)]
pub struct UserInit {
    pub user: User,
    pub devices: Vec<Dev>,
    pub groups: Vec<Group>,
    pub admin: bool,
}

/// 확정 기기가 바뀌는 방식.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Handover {
    /// 소유자가 살아 있는 기기로 넘김(`CommitterChange`, 옛 기기가 넘겨줌).
    Voluntary,
    /// 옛 기기가 사라져 넘겨받지 못함.
    Lost,
    /// 해소기가 넘김(`repair_committer`, 옛 기기 폐기·강등).
    Repair,
}

/// 변동 하나.
#[derive(Clone, Debug, Serialize)]
pub enum Op {
    /// 새 사용자 가입(기기 1~2대, 볼 수 있는 비공개 키 그룹).
    Join {
        user: User,
        devices: Vec<Dev>,
        groups: Vec<Group>,
    },
    /// 탈퇴·추방(그 사용자 기기 전부).
    Leave {
        user: User,
    },
    /// 밴 물결 한 묶음.
    Ban {
        users: Vec<User>,
    },
    DeviceAdd {
        user: User,
        device: Dev,
    },
    /// 기기 폐기. 확정 기기였으면 해소기가 `repair`로 넘깁니다.
    DeviceRevoke {
        user: User,
        device: Dev,
        repair: Option<Dev>,
    },
    /// 비공개 키 그룹 열람 권한을 얻거나 잃음(역할 변경).
    GroupFlip {
        user: User,
        group: Group,
        gain: bool,
    },
    /// 확정 기기 바뀜.
    Committer {
        to: Dev,
        how: Handover,
    },
    /// 관리자 권한을 잃음(멤버로 남음). 그 사용자 기기가 확정 기기면 해소기가 `to`로 넘깁니다. 비공개 키 그룹은
    /// 역할로 받은 `keep`만 남습니다.
    Demote {
        user: User,
        to: Dev,
        keep: Vec<Group>,
    },
    /// 예비 표 보유 기기 지정(소유자만).
    Backup {
        device: Option<Dev>,
    },
    Ownership {
        to: User,
    },
    /// 갈라짐 시작: B쪽 기기들과 그쪽에서 소유자가 정한 새 확정 기기.
    PartitionStart {
        side_b: Vec<Dev>,
        committer_b: Dev,
    },
    /// 갈라짐 끝(합치기). 합친 상태에서는 B쪽 확정 기기 변경이 이깁니다.
    PartitionEnd,
}

/// 변동이 어느 쪽에서 일어났는지(갈라짐 동안만 A·B).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Side {
    Both,
    A,
    B,
}

#[derive(Clone, Debug, Serialize)]
pub struct Event {
    pub at_ms: u64,
    pub side: Side,
    pub op: Op,
}

/// 기록 전체.
#[derive(Clone, Debug, Serialize)]
pub struct Trace {
    pub churn: Churn,
    pub initial: Vec<UserInit>,
    pub owner: User,
    pub committer: Dev,
    /// 비공개 키 그룹 수(번호 1..=groups).
    pub groups: usize,
    pub events: Vec<Event>,
    pub end_ms: u64,
}

impl Trace {
    /// 처음 기기 수.
    pub fn initial_devices(&self) -> usize {
        self.initial.iter().map(|u| u.devices.len()).sum()
    }

    /// 종류별 변동 수(표에 찍는 용도).
    pub fn counts(&self) -> BTreeMap<&'static str, usize> {
        let mut m = BTreeMap::new();
        for e in &self.events {
            let k = match &e.op {
                Op::Join { .. } => "가입",
                Op::Leave { .. } => "탈퇴·추방",
                Op::Ban { .. } => "밴 묶음",
                Op::DeviceAdd { .. } => "기기 추가",
                Op::DeviceRevoke { .. } => "기기 폐기",
                Op::GroupFlip { .. } => "권한 바뀜",
                Op::Committer { .. } => "확정 기기 변경",
                Op::Demote { .. } => "강등",
                Op::Backup { .. } => "예비 기기",
                Op::Ownership { .. } => "소유권 이전",
                Op::PartitionStart { .. } | Op::PartitionEnd => "갈라짐",
            };
            *m.entry(k).or_default() += 1;
        }
        m
    }
}

/// 무작위 변동 종류(시각을 먼저 뽑고 대상은 시간 순서로 걸으며 고릅니다).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Tag {
    Join,
    Leave,
    DeviceAdd,
    Revoke,
    Flip,
    /// 예비 기기 지정(관리자 a의 둘째 기기, 0은 소유자).
    Backup(u8),
    Handover(u8),
    /// 관리자 a 강등(그 기기가 확정 기기면 소유자 기기로 넘어감).
    Demote(u8),
    RevokeCommitter,
    /// 관리자 a의 첫 기기 폐기(넘겨준 뒤라 확정 기기는 아닌 표 보유 기기).
    RevokeHolder(u8),
    /// 지금 예비 기기 폐기.
    RevokeBackup,
    PartitionStart,
    PartitionEnd,
    Lost,
    Ban(u8),
    Ownership,
}

/// 생성 중인 멤버 상태(대상 고르기용).
struct Gen {
    rng: StdRng,
    next_dev: Dev,
    next_user: User,
    devs: HashMap<User, Vec<Dev>>,
    groups: HashMap<User, BTreeSet<Group>>,
    /// 탈퇴·밴·폐기·권한 바뀜의 대상이 될 수 있는 사용자(소유자·관리자 제외)와 그 위치.
    pool: Vec<User>,
    pos: HashMap<User, usize>,
    fractions: Vec<f64>,
    second_device: f64,
}

impl Gen {
    fn new_devices(&mut self, n: usize) -> Vec<Dev> {
        (0..n)
            .map(|_| {
                let d = self.next_dev;
                self.next_dev += 1;
                d
            })
            .collect()
    }

    fn pick_groups(&mut self) -> Vec<Group> {
        let fr = self.fractions.clone();
        fr.iter()
            .enumerate()
            .filter(|(_, f)| self.rng.gen_bool(**f))
            .map(|(g, _)| (g + 1) as Group)
            .collect()
    }

    fn add_pool(&mut self, u: User) {
        self.pos.insert(u, self.pool.len());
        self.pool.push(u);
    }

    fn take_pool(&mut self, u: User) {
        if let Some(i) = self.pos.remove(&u) {
            let last = self.pool.pop().expect("위치가 있으면 비어 있지 않음");
            if last != u {
                self.pool[i] = last;
                self.pos.insert(last, i);
            }
        }
    }

    fn random_pool(&mut self) -> Option<User> {
        if self.pool.is_empty() {
            return None;
        }
        let i = self.rng.gen_range(0..self.pool.len());
        Some(self.pool[i])
    }

    fn join(&mut self) -> Op {
        let user = self.next_user;
        self.next_user += 1;
        let n = if self.rng.gen_bool(self.second_device) {
            2
        } else {
            1
        };
        let devices = self.new_devices(n);
        let groups = self.pick_groups();
        self.devs.insert(user, devices.clone());
        self.groups.insert(user, groups.iter().copied().collect());
        Op::Join {
            user,
            devices,
            groups,
        }
    }

    fn remove(&mut self, u: User) {
        self.take_pool(u);
        self.devs.remove(&u);
        self.groups.remove(&u);
    }
}

/// 기록을 만듭니다. 같은 `Churn`이면 늘 같은 기록입니다.
pub fn generate(c: &Churn) -> Trace {
    let users = users_for(c.devices);
    let end = (c.days * DAY_MS as f64) as u64;
    let mut g = Gen {
        rng: StdRng::seed_from_u64(c.seed),
        next_dev: 0,
        next_user: users as User,
        devs: HashMap::new(),
        groups: HashMap::new(),
        pool: Vec::new(),
        pos: HashMap::new(),
        fractions: c.private_groups.clone(),
        second_device: c.second_device,
    };
    let admins = c.admins.min(users.saturating_sub(2));
    let all_groups: Vec<Group> = (1..=c.private_groups.len() as Group).collect();
    let mut initial = Vec::with_capacity(users);
    for u in 0..users as User {
        let protected = (u as usize) <= admins;
        // 소유자·관리자는 기기 2대(확정 기기 폐기 사건에서 다른 기기가 남도록).
        let n = if protected || g.rng.gen_bool(c.second_device) {
            2
        } else {
            1
        };
        let devices = g.new_devices(n);
        // 관리자도 역할로 받은 그룹을 뽑아 둡니다(강등되면 이것만 남음). 관리자인 동안은 모든 그룹을 봅니다.
        let role_groups = g.pick_groups();
        g.devs.insert(u, devices.clone());
        g.groups.insert(u, role_groups.iter().copied().collect());
        if !protected {
            g.add_pool(u);
        }
        initial.push(UserInit {
            user: u,
            devices,
            groups: if protected {
                all_groups.clone()
            } else {
                role_groups
            },
            admin: protected && u != 0,
        });
    }
    let owner: User = 0;
    let committer = initial[0].devices[0];

    // 시각 뽑기.
    let mut sched: Vec<(u64, Tag)> = Vec::new();
    let count = |rate: f64| (rate * c.days).round() as usize;
    for (rate, tag) in [
        (c.joins_per_day, Tag::Join),
        (c.removals_per_day, Tag::Leave),
        (c.device_adds_per_day, Tag::DeviceAdd),
        (c.revocations_per_day, Tag::Revoke),
        (c.flips_per_day, Tag::Flip),
    ] {
        for _ in 0..count(rate) {
            sched.push((g.rng.gen_range(0..end.max(1)), tag));
        }
    }
    let at = |f: f64| (end as f64 * f) as u64;
    let partition_ms = c.partition_ms.min(end / 10).max(1);
    let special = c.committer_events && admins >= 8;
    if special {
        // 표 보유 기기(마지막 전체 재키 뒤 확정 기기였던 기기 + 예비 기기)가 빠지는 길을 모두 지납니다: 지금 확정
        // 기기의 강등·폐기, 넘겨준 옛 확정 기기의 강등·폐기, 예비 기기 교체·폐기.
        sched.push((at(0.05), Tag::Backup(0)));
        sched.push((at(0.10), Tag::Handover(1)));
        sched.push((at(0.20), Tag::Demote(1)));
        sched.push((at(0.25), Tag::Handover(6)));
        sched.push((at(0.28), Tag::Handover(2)));
        sched.push((at(0.31), Tag::Demote(6)));
        sched.push((at(0.34), Tag::Handover(7)));
        sched.push((at(0.36), Tag::Handover(2)));
        sched.push((at(0.38), Tag::RevokeHolder(7)));
        sched.push((at(0.40), Tag::RevokeCommitter));
        sched.push((at(0.45), Tag::Backup(8)));
        sched.push((at(0.50), Tag::PartitionStart));
        sched.push((at(0.50) + partition_ms, Tag::PartitionEnd));
        sched.push((at(0.60), Tag::Lost));
        sched.push((at(0.65), Tag::RevokeBackup));
        sched.push((at(0.80), Tag::Handover(5)));
        sched.push((at(0.80) + 1, Tag::Ownership));
    }
    let ban_events = c.mass_ban_events.max(1);
    if c.mass_ban > 0 {
        for i in 0..ban_events {
            sched.push((at(0.70) + 700 * i as u64, Tag::Ban(i as u8)));
        }
    }
    sched.sort();

    let admin_dev = |a: usize, i: usize| initial[a].devices[i];
    let partition = (at(0.50), at(0.50) + partition_ms);
    let in_partition = |t: u64| special && t >= partition.0 && t < partition.1;
    let mut events = Vec::with_capacity(sched.len());
    let mut current = committer;
    let mut backup: Option<(User, Dev)> = None;
    // 갈라짐 동안 가입한 사용자는 합친 뒤에야 다른 변동의 대상이 됩니다(한쪽 상태에 없는 사용자를 건드리지 않게).
    let mut joined_in_partition: Vec<User> = Vec::new();
    let per_ban = c.mass_ban.div_ceil(ban_events);
    let mut banned = 0usize;
    for (t, tag) in sched {
        let side = if in_partition(t) && matches!(tag, Tag::Join | Tag::Leave) {
            if g.rng.gen_bool(c.partition_share) {
                Side::B
            } else {
                Side::A
            }
        } else {
            Side::Both
        };
        if in_partition(t) && !matches!(tag, Tag::Join | Tag::Leave | Tag::PartitionStart) {
            continue;
        }
        let op = match tag {
            Tag::Join => {
                let op = g.join();
                if let Op::Join { user, .. } = &op {
                    if in_partition(t) {
                        joined_in_partition.push(*user);
                    } else {
                        g.add_pool(*user);
                    }
                }
                op
            }
            Tag::Leave => {
                let Some(u) = g.random_pool() else { continue };
                g.remove(u);
                Op::Leave { user: u }
            }
            Tag::DeviceAdd => {
                let mut found = None;
                for _ in 0..4 {
                    if let Some(u) = g.random_pool()
                        && g.devs.get(&u).is_some_and(|d| d.len() == 1)
                    {
                        found = Some(u);
                        break;
                    }
                }
                let Some(u) = found else { continue };
                let d = g.new_devices(1)[0];
                g.devs.entry(u).or_default().push(d);
                Op::DeviceAdd { user: u, device: d }
            }
            Tag::Revoke => {
                let mut found = None;
                for _ in 0..4 {
                    if let Some(u) = g.random_pool()
                        && g.devs.get(&u).is_some_and(|d| d.len() == 2)
                    {
                        found = Some(u);
                        break;
                    }
                }
                let Some(u) = found else { continue };
                let d = g.devs.get_mut(&u).and_then(|v| v.pop()).expect("기기 2대");
                Op::DeviceRevoke {
                    user: u,
                    device: d,
                    repair: None,
                }
            }
            Tag::Flip => {
                if c.private_groups.is_empty() {
                    continue;
                }
                let Some(u) = g.random_pool() else { continue };
                let group = g.rng.gen_range(1..=c.private_groups.len()) as Group;
                let set = g.groups.entry(u).or_default();
                let gain = !set.contains(&group);
                if gain {
                    set.insert(group);
                } else {
                    set.remove(&group);
                }
                Op::GroupFlip {
                    user: u,
                    group,
                    gain,
                }
            }
            Tag::Backup(a) => {
                let device = admin_dev(a as usize, 1);
                backup = Some((initial[a as usize].user, device));
                Op::Backup {
                    device: Some(device),
                }
            }
            Tag::Handover(a) => {
                current = admin_dev(a as usize, 0);
                Op::Committer {
                    to: current,
                    how: Handover::Voluntary,
                }
            }
            Tag::Demote(a) => {
                // 그 관리자의 기기가 확정 기기면(관리자 1) 해소기가 소유자 기기로 넘깁니다. 아니면(관리자 6) 넘겨준 옛
                // 확정 기기라 표 보유 기기만 빠집니다.
                let user = initial[a as usize].user;
                if initial[a as usize].devices.contains(&current) {
                    current = admin_dev(0, 0);
                }
                let keep: Vec<Group> = g
                    .groups
                    .get(&user)
                    .map(|s| s.iter().copied().collect())
                    .unwrap_or_default();
                Op::Demote {
                    user,
                    to: current,
                    keep,
                }
            }
            Tag::RevokeHolder(a) => {
                let user = initial[a as usize].user;
                let device = admin_dev(a as usize, 0);
                if device == current {
                    continue;
                }
                if let Some(v) = g.devs.get_mut(&user) {
                    v.retain(|d| *d != device);
                }
                Op::DeviceRevoke {
                    user,
                    device,
                    repair: None,
                }
            }
            Tag::RevokeBackup => {
                let Some((user, device)) = backup.take() else {
                    continue;
                };
                if device == current {
                    continue;
                }
                if let Some(v) = g.devs.get_mut(&user) {
                    v.retain(|d| *d != device);
                }
                Op::DeviceRevoke {
                    user,
                    device,
                    repair: None,
                }
            }
            Tag::RevokeCommitter => {
                let dev = current;
                let user = initial[2].user;
                current = admin_dev(0, 0);
                if let Some(v) = g.devs.get_mut(&user) {
                    v.retain(|d| *d != dev);
                }
                Op::DeviceRevoke {
                    user,
                    device: dev,
                    repair: Some(current),
                }
            }
            Tag::PartitionStart => {
                // B쪽: 소유자의 둘째 기기(거기서 확정 기기를 바꿈), 관리자 3, 무작위 사용자들.
                let mut side_b: BTreeSet<Dev> = BTreeSet::new();
                side_b.insert(admin_dev(0, 1));
                side_b.extend(initial[3].devices.iter().copied());
                let want = (g.pool.len() as f64 * c.partition_share) as usize;
                let mut pool = g.pool.clone();
                pool.shuffle(&mut g.rng);
                for u in pool.into_iter().take(want) {
                    if let Some(d) = g.devs.get(&u) {
                        side_b.extend(d.iter().copied());
                    }
                }
                current = admin_dev(3, 0);
                Op::PartitionStart {
                    side_b: side_b.into_iter().collect(),
                    committer_b: current,
                }
            }
            Tag::PartitionEnd => {
                for u in std::mem::take(&mut joined_in_partition) {
                    g.add_pool(u);
                }
                Op::PartitionEnd
            }
            Tag::Lost => {
                current = admin_dev(4, 0);
                Op::Committer {
                    to: current,
                    how: Handover::Lost,
                }
            }
            Tag::Ban(_) => {
                let mut users = Vec::new();
                while users.len() < per_ban && banned < c.mass_ban {
                    let Some(u) = g.random_pool() else { break };
                    g.remove(u);
                    users.push(u);
                    banned += 1;
                }
                if users.is_empty() {
                    continue;
                }
                users.sort_unstable();
                Op::Ban { users }
            }
            Tag::Ownership => Op::Ownership {
                to: initial[5].user,
            },
        };
        events.push(Event { at_ms: t, side, op });
    }
    Trace {
        churn: c.clone(),
        initial,
        owner,
        committer,
        groups: c.private_groups.len(),
        events,
        end_ms: end,
    }
}

/// 멤버십 변화. 키 모형은 이것만 보고 키를 바꿉니다.
#[derive(Clone, Debug, Default)]
pub struct Delta {
    /// (그룹, 기기): 새로 자격을 얻음.
    pub added: Vec<(Group, Dev)>,
    /// (그룹, 기기): 자격을 잃음.
    pub removed: Vec<(Group, Dev)>,
    /// 확정 기기 바뀜: (예전, 새, 방식).
    pub committer: Option<(Dev, Dev, Handover)>,
    /// 예비 기기가 바뀜(새 값).
    pub backup: Option<Option<Dev>>,
    /// 이 변화로 표 보유 자격(확정 기기·예비 기기였던 권한)을 잃는 기기 후보: 크루에서 빠진 기기, 강등된 사용자의
    /// 기기, 교체된 예비 기기.
    pub demoted: Vec<Dev>,
}

impl Delta {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.committer.is_none()
            && self.backup.is_none()
            && self.demoted.is_empty()
    }
}

/// 지금 멤버십(해소된 상태에서 키 그룹마다 자격 있는 기기).
#[derive(Clone, Debug)]
pub struct World {
    /// 그룹마다 자격 있는 기기. `[0]`은 크루(모든 활성 기기).
    pub groups: Vec<BTreeSet<Dev>>,
    user_devs: BTreeMap<User, Vec<Dev>>,
    user_groups: BTreeMap<User, BTreeSet<Group>>,
    dev_user: HashMap<Dev, User>,
    pub committer: Dev,
    pub backup: Option<Dev>,
    pub owner: User,
}

impl World {
    pub fn new(t: &Trace) -> Self {
        let mut w = World {
            groups: vec![BTreeSet::new(); t.groups + 1],
            user_devs: BTreeMap::new(),
            user_groups: BTreeMap::new(),
            dev_user: HashMap::new(),
            committer: t.committer,
            backup: None,
            owner: t.owner,
        };
        for u in &t.initial {
            let mut d = Delta::default();
            w.join(u.user, &u.devices, &u.groups, &mut d);
        }
        w
    }

    pub fn users(&self) -> usize {
        self.user_devs.len()
    }

    pub fn devices(&self) -> usize {
        self.groups[CREW as usize].len()
    }

    pub fn group(&self, g: Group) -> &BTreeSet<Dev> {
        &self.groups[g as usize]
    }

    pub fn user_of(&self, d: Dev) -> Option<User> {
        self.dev_user.get(&d).copied()
    }

    pub fn devices_of(&self, u: User) -> &[Dev] {
        self.user_devs.get(&u).map_or(&[], |v| v.as_slice())
    }

    /// 사용자마다의 기기(정렬됨).
    pub fn user_devices(&self) -> impl Iterator<Item = (User, &[Dev])> {
        self.user_devs.iter().map(|(u, d)| (*u, d.as_slice()))
    }

    fn join(&mut self, user: User, devices: &[Dev], groups: &[Group], d: &mut Delta) {
        self.user_devs.insert(user, devices.to_vec());
        self.user_groups
            .insert(user, groups.iter().copied().collect());
        for &dev in devices {
            self.dev_user.insert(dev, user);
            self.add(CREW, dev, d);
            for &g in groups {
                self.add(g, dev, d);
            }
        }
    }

    fn add(&mut self, g: Group, dev: Dev, d: &mut Delta) {
        if self.groups[g as usize].insert(dev) {
            d.added.push((g, dev));
        }
    }

    fn drop_dev(&mut self, g: Group, dev: Dev, d: &mut Delta) {
        if self.groups[g as usize].remove(&dev) {
            d.removed.push((g, dev));
        }
    }

    fn remove_device(&mut self, user: User, dev: Dev, d: &mut Delta) {
        let groups: Vec<Group> = self
            .user_groups
            .get(&user)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default();
        self.drop_dev(CREW, dev, d);
        for g in groups {
            self.drop_dev(g, dev, d);
        }
        self.dev_user.remove(&dev);
        if let Some(v) = self.user_devs.get_mut(&user) {
            v.retain(|x| *x != dev);
        }
        d.demoted.push(dev);
        if self.backup == Some(dev) {
            self.backup = None;
            d.backup = Some(None);
        }
    }

    fn remove_user(&mut self, user: User, d: &mut Delta) {
        for dev in self.devices_of(user).to_vec() {
            self.remove_device(user, dev, d);
        }
        self.user_devs.remove(&user);
        self.user_groups.remove(&user);
    }

    /// 변동 하나를 적용하고 멤버십 변화를 돌려줍니다.
    pub fn apply(&mut self, op: &Op) -> Delta {
        let mut d = Delta::default();
        match op {
            Op::Join {
                user,
                devices,
                groups,
            } => self.join(*user, devices, groups, &mut d),
            Op::Leave { user } => self.remove_user(*user, &mut d),
            Op::Ban { users } => {
                for u in users {
                    self.remove_user(*u, &mut d);
                }
            }
            Op::DeviceAdd { user, device } => {
                if self.user_devs.contains_key(user) {
                    self.user_devs.entry(*user).or_default().push(*device);
                    self.dev_user.insert(*device, *user);
                    self.add(CREW, *device, &mut d);
                    let groups: Vec<Group> = self.user_groups[user].iter().copied().collect();
                    for g in groups {
                        self.add(g, *device, &mut d);
                    }
                }
            }
            Op::DeviceRevoke {
                user,
                device,
                repair,
            } => {
                self.remove_device(*user, *device, &mut d);
                if self.committer == *device {
                    let to = repair.expect("확정 기기 폐기에는 넘길 기기가 있음");
                    d.committer = Some((*device, to, Handover::Repair));
                    self.committer = to;
                }
            }
            Op::GroupFlip { user, group, gain } => {
                if let Some(set) = self.user_groups.get_mut(user) {
                    if *gain {
                        set.insert(*group);
                    } else {
                        set.remove(group);
                    }
                    for dev in self.devices_of(*user).to_vec() {
                        if *gain {
                            self.add(*group, dev, &mut d);
                        } else {
                            self.drop_dev(*group, dev, &mut d);
                        }
                    }
                }
            }
            Op::Committer { to, how } => {
                d.committer = Some((self.committer, *to, *how));
                self.committer = *to;
            }
            Op::Demote { user, to, keep } => {
                let devs = self.devices_of(*user).to_vec();
                if devs.contains(&self.committer) {
                    d.committer = Some((self.committer, *to, Handover::Repair));
                    self.committer = *to;
                }
                // ADMINISTRATOR가 주던 열람을 잃습니다: 역할로 받은 그룹만 남습니다.
                if let Some(set) = self.user_groups.get_mut(user) {
                    let lost: Vec<Group> =
                        set.iter().filter(|g| !keep.contains(g)).copied().collect();
                    set.retain(|g| keep.contains(g));
                    for g in lost {
                        for &dev in &devs {
                            self.drop_dev(g, dev, &mut d);
                        }
                    }
                }
                d.demoted.extend(devs);
            }
            Op::Backup { device } => {
                if let Some(old) = self.backup
                    && Some(old) != *device
                {
                    d.demoted.push(old);
                }
                self.backup = *device;
                d.backup = Some(*device);
            }
            Op::Ownership { to } => self.owner = *to,
            Op::PartitionStart { committer_b, .. } => {
                d.committer = Some((self.committer, *committer_b, Handover::Lost));
                self.committer = *committer_b;
            }
            Op::PartitionEnd => {}
        }
        d
    }

    /// `keyed`(어느 키 모형이 따라온 멤버십)에서 이 멤버십으로 가는 변화(갈라짐 합치기 때).
    pub fn diff_from(&self, keyed: &World) -> Delta {
        let mut d = Delta::default();
        for (g, (now, old)) in self.groups.iter().zip(&keyed.groups).enumerate() {
            let g = g as Group;
            d.added.extend(now.difference(old).map(|dev| (g, *dev)));
            d.removed.extend(old.difference(now).map(|dev| (g, *dev)));
        }
        d.demoted = keyed.groups[CREW as usize]
            .difference(&self.groups[CREW as usize])
            .copied()
            .collect();
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_trace_and_consistent_world() {
        let mut c = Churn::new(300, 2.0, 7);
        c.joins_per_day = 20.0;
        c.removals_per_day = 20.0;
        let a = generate(&c);
        let b = generate(&c);
        assert_eq!(a.events.len(), b.events.len());
        assert_eq!(format!("{:?}", a.events), format!("{:?}", b.events));
        let mut w = World::new(&a);
        // 관리자는 모든 비공개 그룹을 봅니다.
        for g in &w.groups[1..=a.groups] {
            assert!(g.contains(&a.initial[1].devices[0]));
        }
        let mut former: BTreeSet<Dev> = BTreeSet::new();
        let (mut holder_revoked, mut holder_demoted, mut backup_dropped) = (false, false, false);
        for e in &a.events {
            let before = w.committer;
            let d = w.apply(&e.op);
            assert!(w.groups[CREW as usize].contains(&w.committer), "{e:?}");
            match &e.op {
                Op::DeviceRevoke { device, .. } if former.contains(device) => {
                    holder_revoked |= *device != before;
                }
                Op::Demote { user, .. } => {
                    let devs: Vec<Dev> = a.initial[*user as usize].devices.clone();
                    holder_demoted |= devs.iter().any(|x| former.contains(x) && *x != before);
                    // 강등된 관리자는 역할로 받지 않은 비공개 그룹을 잃습니다.
                    assert!(d.removed.iter().all(|(g, _)| *g != CREW), "{d:?}");
                }
                _ => {}
            }
            backup_dropped |= d.backup == Some(None)
                || matches!(e.op, Op::Backup { .. }) && !d.demoted.is_empty();
            former.insert(before);
        }
        assert!(holder_revoked && holder_demoted && backup_dropped);
        let counts = a.counts();
        for k in [
            "강등",
            "기기 폐기",
            "확정 기기 변경",
            "갈라짐",
            "밴 묶음",
            "소유권 이전",
        ] {
            assert!(counts.contains_key(k), "{k} 없음: {counts:?}");
        }
    }
}
