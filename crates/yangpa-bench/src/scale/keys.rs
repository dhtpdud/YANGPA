//! 키 관리 방식 모형: 바꾸기 전 MLS([`LegacyMls`]), 원안 하위 그룹([`BriefSubgroups`]), 수정판 키 트리([`KeyTreeModel`]).
//! 앞 둘은 데몬에 없는 구조로, 키 그룹으로 바꾼 까닭을 같은 기록에서 견주려고 모형으로만 남깁니다.
//!
//! 세 방식을 같은 변동 기록([`crate::scale::trace`])에 돌려, 키 이벤트마다 이벤트 수·발행 바이트·기기당 내려받기
//! (그 그룹 기기 / 그 밖 크루 기기 / 대상 기기)·X-Wing 봉인 수·서명 수·최대 본문을 셉니다([`OpCost`]). 동시에
//! 누가 어떤 키를 얻을 수 있는지 기호로 따라가([`Knowledge`]) 불변식 I1~I4를 검사합니다.
//!
//! 바이트는 모형 값입니다: 이벤트 = 고정 부분(머리글·혼합 서명·AEAD, 실제 이벤트로 맞춤) + Padmé(본문), X-Wing 봉인
//! 1,227B(32B 키 하나), 대칭 감싸기 80B(표지 8B + 무작위 nonce 24B + 키 32B + 태그 16B), MLS 커밋·웰컴은 MLS를
//! 빼기 전 마지막 openmls 0.9 측정(performance.md "키 관리 세 방식")의 기기당 기울기. CPU는 측정한 명령 수(`sealed_box_8` 13.6M을 8로 나눈 X-Wing 한 번,
//! `event_sign_verify` 4.10M)에 횟수를 곱한 어림입니다.

mod brief;
pub mod knowledge;
mod legacy;
mod tree;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use serde::Serialize;
use yangpa_crypto::pad::padme_len;

pub use brief::BriefSubgroups;
pub use knowledge::{ALL_VIEWS, CheckOpts, InvariantReport, KeyId, Knowledge, Lane, Scope, View};
pub use legacy::LegacyMls;
pub use tree::KeyTreeModel;

use crate::scale::trace::{CREW, Delta, Dev, Event, Group, Op, Side, Trace, World};

/// `sealed_box_8`(기기 8대 앞 봉인 + 한 대 열기)의 명령 수와 시간. 8로 나눠 X-Wing 봉인 하나로 봅니다.
pub const SEALED_BOX_8_INSTR: u64 = 13_639_931;
pub const SEALED_BOX_8_SECS: f64 = 1.13e-3;
/// `event_sign_verify`(혼합 서명 + 검증)의 명령 수와 시간.
pub const EVENT_SIGN_INSTR: u64 = 4_102_702;
pub const EVENT_SIGN_SECS: f64 = 392e-6;

/// 크기·시간 조건.
#[derive(Clone, Debug, Serialize)]
pub struct CostModel {
    /// 이벤트 고정 부분(머리글, 혼합 서명 1,659B, AEAD nonce·태그, 직렬화).
    pub event_fixed: usize,
    /// 32B 키 하나를 X-Wing으로 봉인한 크기.
    pub xwing_seal: usize,
    /// 대칭 감싸기 하나.
    pub wrap: usize,
    /// 옛 MLS(마지막 openmls 측정): 경로가 있는 커밋의 기기당 크기, 웰컴(트리 포함)의 기기당 크기, 경로 없는 추가 커밋.
    pub mls_commit_per_device: f64,
    pub mls_welcome_per_device: f64,
    pub mls_add_commit: usize,
    /// 원안 하위 그룹 상한.
    pub subgroup_cap: usize,
    /// 직접 인코딩 상한과, 트리에서 직접으로 돌아오는 크기.
    pub direct_max: usize,
    pub direct_back: usize,
    /// 묶음 창: 기본, 제거가 이어질 때 최대.
    pub window_ms: u64,
    pub window_max_ms: u64,
    /// 정기 전체 재키, 시간 회전(또는 가입 수), 체크포인트(또는 상태 이벤트 수).
    pub full_rekey_ms: u64,
    pub time_rotation_ms: u64,
    pub time_rotation_joins: u32,
    pub checkpoint_ms: u64,
    pub checkpoint_events: u32,
}

impl Default for CostModel {
    fn default() -> Self {
        CostModel {
            event_fixed: EVENT_FIXED,
            xwing_seal: 1_227,
            wrap: 80,
            mls_commit_per_device: 1_240.0,
            mls_welcome_per_device: 1_480.0,
            mls_add_commit: 2_927,
            subgroup_cap: 256,
            direct_max: 256,
            direct_back: 160,
            window_ms: 300,
            window_max_ms: 2_000,
            full_rekey_ms: 7 * crate::scale::trace::DAY_MS,
            time_rotation_ms: crate::scale::trace::DAY_MS,
            time_rotation_joins: 1_000,
            checkpoint_ms: crate::scale::trace::DAY_MS,
            checkpoint_events: 1_000,
        }
    }
}

/// 실제 이벤트(본문 L바이트, 무작위라 압축 안 됨)의 전송 크기 = 이것 + Padmé(4 + L). `scale` 시험이 맞춰 봅니다.
pub const EVENT_FIXED: usize = 1_909;

impl CostModel {
    /// 본문 `body`바이트 이벤트의 전송 크기.
    pub fn event(&self, body: usize) -> u64 {
        (self.event_fixed + padme_len(4 + body)) as u64
    }

    /// `payload`바이트를 X-Wing으로 봉인한 크기.
    pub fn seal(&self, payload: usize) -> usize {
        self.xwing_seal - 32 + payload
    }
}

/// 비용을 모으는 갈래.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum CostClass {
    #[default]
    Setup,
    Join,
    DeviceAdd,
    FlipIn,
    Removal,
    Revoke,
    FlipOut,
    Ban,
    Committer,
    Backup,
    TimeRotation,
    FullRekey,
    ModeSwitch,
    Split,
    Merge,
}

impl CostClass {
    pub fn label(self) -> &'static str {
        match self {
            CostClass::Setup => "처음 구성",
            CostClass::Join => "가입",
            CostClass::DeviceAdd => "기기 추가",
            CostClass::FlipIn => "권한 얻음",
            CostClass::Removal => "탈퇴·추방",
            CostClass::Revoke => "기기 폐기",
            CostClass::FlipOut => "권한 잃음",
            CostClass::Ban => "밴 물결",
            CostClass::Committer => "확정 기기 변경",
            CostClass::Backup => "예비 기기",
            CostClass::TimeRotation => "시간 회전",
            CostClass::FullRekey => "전체 재키",
            CostClass::ModeSwitch => "인코딩 바뀜",
            CostClass::Split => "하위 그룹 나누기",
            CostClass::Merge => "갈라짐 합치기",
        }
    }

    /// 한 묶음에 여러 원인이 섞이면 이 순서가 큰 쪽으로 셉니다(제거가 가입보다 앞).
    fn rank(self) -> u8 {
        match self {
            CostClass::Join => 1,
            CostClass::DeviceAdd => 2,
            CostClass::FlipIn => 3,
            CostClass::FlipOut => 4,
            CostClass::Removal => 5,
            CostClass::Revoke => 6,
            CostClass::Ban => 7,
            _ => 8,
        }
    }
}

/// 변동이 키 그룹에 주는 원인.
pub fn cause_of(op: &Op, added: bool) -> CostClass {
    match op {
        Op::Join { .. } => CostClass::Join,
        Op::DeviceAdd { .. } => CostClass::DeviceAdd,
        Op::Leave { .. } => CostClass::Removal,
        Op::Ban { .. } => CostClass::Ban,
        Op::DeviceRevoke { .. } => CostClass::Revoke,
        Op::GroupFlip { .. } if added => CostClass::FlipIn,
        Op::GroupFlip { .. } => CostClass::FlipOut,
        // 강등은 ADMINISTRATOR가 주던 열람을 잃는 것이라 권한 잃음과 같습니다.
        Op::Demote { .. } => CostClass::FlipOut,
        _ => CostClass::Merge,
    }
}

/// 이벤트 하나를 누가 받는지(기기 수).
#[derive(Clone, Copy, Debug, Default)]
pub struct Reach {
    /// 바뀐 그룹의 기기.
    pub own: usize,
    /// 그 밖의 크루 기기(공간 스트림에 실리면).
    pub others: usize,
    /// 대상 기기(가입자의 Inbox, 웰컴 대상).
    pub target: usize,
}

/// 키 작업 하나(한 그룹, 한 묶음)의 비용.
#[derive(Clone, Debug, Default, Serialize)]
pub struct OpCost {
    pub class: CostClass,
    pub group: Group,
    pub events: u64,
    /// 발행한 바이트(이벤트·블롭 전부, gossip 복사 전).
    pub published: u64,
    /// 기기 하나가 받는 바이트: 그 그룹 기기, 그 밖 크루 기기, 대상 기기.
    pub own: u64,
    pub others: u64,
    pub target: u64,
    /// 모든 기기가 받는 합.
    pub total_download: u64,
    /// 직접 요청으로 넘기는 바이트(표 넘겨주기).
    pub direct: u64,
    pub xwing: u64,
    pub signatures: u64,
    pub max_body: u64,
    /// 레인 한도를 넘은 이벤트 수(I4).
    pub over_cap: u64,
}

impl OpCost {
    pub fn new(class: CostClass, group: Group) -> Self {
        OpCost {
            class,
            group,
            ..Default::default()
        }
    }

    /// 서명된 이벤트 하나를 냄.
    pub fn event(
        &mut self,
        kn: &mut Knowledge,
        m: &CostModel,
        what: &'static str,
        lane: Lane,
        body: usize,
        reach: Reach,
    ) {
        kn.body(what, lane, body);
        if body > lane.cap() {
            self.over_cap += 1;
        }
        let size = m.event(body);
        self.events += 1;
        self.signatures += 1;
        self.published += size;
        self.max_body = self.max_body.max(body as u64);
        self.receive(size, reach);
    }

    /// 이미 낸 것을 받는 쪽만 셈(블롭 조각 등).
    pub fn receive(&mut self, size: u64, reach: Reach) {
        if reach.own > 0 {
            self.own += size;
        }
        if reach.others > 0 {
            self.others += size;
        }
        if reach.target > 0 {
            self.target += size;
        }
        self.total_download += size * (reach.own + reach.others + reach.target) as u64;
    }

    fn fields(&self) -> [u64; 11] {
        [
            self.events,
            self.published,
            self.own,
            self.others,
            self.target,
            self.total_download,
            self.direct,
            self.xwing,
            self.signatures,
            self.max_body,
            self.over_cap,
        ]
    }

    fn from_fields(class: CostClass, group: Group, f: [u64; 11]) -> Self {
        OpCost {
            class,
            group,
            events: f[0],
            published: f[1],
            own: f[2],
            others: f[3],
            target: f[4],
            total_download: f[5],
            direct: f[6],
            xwing: f[7],
            signatures: f[8],
            max_body: f[9],
            over_cap: f[10],
        }
    }
}

/// 한 갈래(종류 × 크루/비공개)의 모음.
#[derive(Clone, Debug, Serialize)]
pub struct ClassStats {
    pub class: CostClass,
    /// 크루 그룹이면 참, 비공개 키 그룹이면 거짓.
    pub crew: bool,
    pub count: u64,
    pub sum: OpCost,
    pub max: OpCost,
    /// 그룹 기기 하나의 받기 p95.
    pub own_p95: u64,
    #[serde(skip)]
    own_values: Vec<u64>,
}

impl ClassStats {
    fn new(class: CostClass, crew: bool) -> Self {
        ClassStats {
            class,
            crew,
            count: 0,
            sum: OpCost::new(class, 0),
            max: OpCost::new(class, 0),
            own_p95: 0,
            own_values: Vec::new(),
        }
    }

    fn add(&mut self, c: &OpCost) {
        self.count += 1;
        let (s, m, x) = (self.sum.fields(), self.max.fields(), c.fields());
        let mut ns = [0u64; 11];
        let mut nm = [0u64; 11];
        for i in 0..11 {
            ns[i] = s[i] + x[i];
            nm[i] = m[i].max(x[i]);
        }
        self.sum = OpCost::from_fields(self.class, 0, ns);
        self.max = OpCost::from_fields(self.class, 0, nm);
        self.own_values.push(c.own);
    }

    fn finish(&mut self) {
        self.own_values.sort_unstable();
        if let Some(&v) = self
            .own_values
            .get((self.own_values.len() as f64 * 0.95) as usize)
            .or(self.own_values.last())
        {
            self.own_p95 = v;
        }
    }

    /// 평균.
    pub fn mean(&self, f: impl Fn(&OpCost) -> u64) -> f64 {
        f(&self.sum) as f64 / self.count.max(1) as f64
    }
}

/// 확정 기기 CPU 어림.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Cpu {
    pub xwing: u64,
    pub signatures: u64,
    pub instructions: u64,
    pub seconds: f64,
}

impl Cpu {
    fn of(xwing: u64, signatures: u64) -> Self {
        Cpu {
            xwing,
            signatures,
            instructions: xwing * SEALED_BOX_8_INSTR / 8 + signatures * EVENT_SIGN_INSTR,
            seconds: xwing as f64 * SEALED_BOX_8_SECS / 8.0 + signatures as f64 * EVENT_SIGN_SECS,
        }
    }
}

/// 한 방식을 한 기록에 돌린 결과.
#[derive(Clone, Debug, Serialize)]
pub struct KeyRun {
    pub scheme: &'static str,
    pub devices: usize,
    pub users: usize,
    pub days: f64,
    pub ops: usize,
    pub classes: Vec<ClassStats>,
    pub invariants: InvariantReport,
    pub notes: Vec<(String, String)>,
    /// 기록 전체의 확정 기기 CPU(처음 구성 제외)와 그중 가장 무거운 작업 하나.
    pub cpu: Cpu,
    pub cpu_max_op: Cpu,
    /// 비공개 키 그룹에 들지 않은 크루 기기 하나가 하루에 받는 키 바이트(처음 구성 제외).
    pub plain_device_per_day: f64,
    pub elapsed_ms: u64,
}

impl KeyRun {
    pub fn class(&self, class: CostClass, crew: bool) -> Option<&ClassStats> {
        self.classes
            .iter()
            .find(|c| c.class == class && c.crew == crew)
    }
}

/// 방식이 보는 문맥.
pub struct Cx<'a> {
    pub kn: &'a mut Knowledge,
    pub world: &'a World,
    pub model: &'a CostModel,
    /// 지금 시각(밀리초).
    pub now: u64,
}

/// 키 관리 방식.
///
/// 계획의 `apply(op) -> OpCost`와 달리, 묶음 창 때문에 비용이 변동보다 늦게 나오므로 시각을 넘기는 `advance`가
/// 따로 있고, 갈라짐 동안 두 갈래가 한 [`Knowledge`]에 보기를 달리해 쓰므로 모형은 문맥([`Cx`])으로 받습니다.
pub trait KeyScheme {
    fn name(&self) -> &'static str;
    /// 처음 멤버십으로 키를 세웁니다.
    fn init(&mut self, cx: &mut Cx<'_>) -> Vec<OpCost>;
    /// 변동 하나(이미 `cx.world`에 적용됨, 변화는 `d`).
    fn apply(&mut self, cx: &mut Cx<'_>, ev: &Event, d: &Delta) -> Vec<OpCost>;
    /// `cx.now`까지 미뤄 둔 묶음·타이머를 처리합니다.
    fn advance(&mut self, cx: &mut Cx<'_>) -> Vec<OpCost>;
    /// 갈라짐을 합칠 때 이긴 쪽이 합친 멤버십으로 맞춥니다(`d`는 이 쪽이 모르던 변화).
    fn resync(&mut self, cx: &mut Cx<'_>, d: &Delta) -> Vec<OpCost>;
    /// 기록 끝: 남은 묶음을 냅니다.
    fn finish(&mut self, cx: &mut Cx<'_>) -> Vec<OpCost>;
    fn boxed_clone(&self) -> Box<dyn KeyScheme + Send>;
    /// 방식별 참고 숫자(하위 그룹 수 등).
    fn notes(&self) -> Vec<(String, String)>;
}

/// 어느 방식인지.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum SchemeKind {
    LegacyMls,
    BriefSubgroups,
    KeyTree,
}

impl SchemeKind {
    pub const ALL: [SchemeKind; 3] = [
        SchemeKind::LegacyMls,
        SchemeKind::BriefSubgroups,
        SchemeKind::KeyTree,
    ];

    pub fn build(self) -> Box<dyn KeyScheme + Send> {
        match self {
            SchemeKind::LegacyMls => Box::new(LegacyMls::default()),
            SchemeKind::BriefSubgroups => Box::new(BriefSubgroups::default()),
            SchemeKind::KeyTree => Box::new(KeyTreeModel::default()),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SchemeKind::LegacyMls => "옛 MLS",
            SchemeKind::BriefSubgroups => "원안 하위 그룹",
            SchemeKind::KeyTree => "수정판 키 트리",
        }
    }
}

/// 실행 조건.
#[derive(Clone, Copy, Debug)]
pub struct RunOpts {
    /// 기호 모형을 따라갈지(끄면 I4만 셈).
    pub track: bool,
    pub check: CheckOpts,
}

impl Default for RunOpts {
    fn default() -> Self {
        RunOpts {
            track: true,
            check: CheckOpts::default(),
        }
    }
}

/// 갈라짐 동안의 두 갈래.
struct Fork {
    a: Box<dyn KeyScheme + Send>,
    world_a: World,
    world_b: World,
}

fn side_mask(side: Side) -> (View, u8) {
    match side {
        Side::A => (View::A, View::A.bit() | View::Main.bit()),
        Side::B => (View::B, View::B.bit() | View::Main.bit()),
        Side::Both => (View::Main, ALL_VIEWS),
    }
}

/// 자격·표 보유 일정에 변화를 적습니다(기록이 정하는 것이라 방식과 무관).
fn record(kn: &mut Knowledge, d: &Delta) {
    for &(g, dev) in &d.added {
        kn.set_eligible(dev, g, true);
    }
    for &(g, dev) in &d.removed {
        kn.set_eligible(dev, g, false);
    }
    for &dev in &d.demoted {
        if kn.is_table(dev) {
            kn.set_table(dev, false);
        }
    }
    if let Some((_, new, _)) = d.committer {
        kn.set_table(new, true);
    }
    if let Some(Some(b)) = d.backup {
        kn.set_table(b, true);
    }
}

/// 한 방식을 기록 끝까지 돌리고 검사합니다.
pub fn run(kind: SchemeKind, trace: &Trace, model: &CostModel, opts: RunOpts) -> KeyRun {
    run_scheme(kind.build(), kind.label(), trace, model, opts)
}

/// [`run`]과 같되 방식을 직접 받습니다(시험이 일부러 망가뜨린 방식을 넣음).
fn run_scheme(
    mut scheme: Box<dyn KeyScheme + Send>,
    label: &'static str,
    trace: &Trace,
    model: &CostModel,
    opts: RunOpts,
) -> KeyRun {
    let started = Instant::now();
    let mut kn = Knowledge::new(opts.track);
    let mut world = World::new(trace);
    let mut all: Vec<OpCost> = Vec::new();
    kn.begin();
    for (g, set) in world.groups.iter().enumerate() {
        for &dev in set {
            kn.set_eligible(dev, g as Group, true);
        }
    }
    kn.set_table(world.committer, true);
    let setup = scheme.init(&mut Cx {
        kn: &mut kn,
        world: &world,
        model,
        now: 0,
    });
    let mut fork: Option<Fork> = None;
    for ev in &trace.events {
        let now = ev.at_ms;
        if let Some(f) = fork.as_mut() {
            kn.set_view(View::A, View::A.bit() | View::Main.bit());
            all.extend(f.a.advance(&mut Cx {
                kn: &mut kn,
                world: &f.world_a,
                model,
                now,
            }));
            kn.set_view(View::B, View::B.bit() | View::Main.bit());
            all.extend(scheme.advance(&mut Cx {
                kn: &mut kn,
                world: &f.world_b,
                model,
                now,
            }));
        } else {
            all.extend(scheme.advance(&mut Cx {
                kn: &mut kn,
                world: &world,
                model,
                now,
            }));
        }
        kn.begin();
        match (&ev.op, fork.as_mut()) {
            (Op::PartitionStart { .. }, None) => {
                // 갈라짐: 지금 갈래가 A, 복제가 B. B쪽에서 소유자가 확정 기기를 바꿉니다(넘겨받지 못함).
                let a = scheme.boxed_clone();
                let world_a = world.clone();
                let mut world_b = world.clone();
                let d = world_b.apply(&ev.op);
                world.apply(&ev.op);
                kn.set_view(View::B, View::B.bit() | View::Main.bit());
                record(&mut kn, &d);
                all.extend(scheme.apply(
                    &mut Cx {
                        kn: &mut kn,
                        world: &world_b,
                        model,
                        now,
                    },
                    ev,
                    &d,
                ));
                fork = Some(Fork {
                    a,
                    world_a,
                    world_b,
                });
            }
            (Op::PartitionEnd, Some(_)) => {
                // 합친 상태에서는 B쪽 확정 기기 변경이 이깁니다. B 갈래가 A쪽 변화를 받아 맞춥니다.
                let f = fork.take().expect("갈라짐 중");
                kn.set_view(View::Main, ALL_VIEWS);
                kn.merge_views();
                let d = world.diff_from(&f.world_b);
                all.extend(scheme.resync(
                    &mut Cx {
                        kn: &mut kn,
                        world: &world,
                        model,
                        now,
                    },
                    &d,
                ));
            }
            (op, Some(f)) => {
                world.apply(op);
                let (view, mask) = side_mask(ev.side);
                kn.set_view(view, mask);
                if ev.side != Side::B {
                    let d = f.world_a.apply(op);
                    if ev.side == Side::Both {
                        kn.set_view(View::A, View::A.bit() | View::Main.bit());
                    }
                    record(&mut kn, &d);
                    all.extend(f.a.apply(
                        &mut Cx {
                            kn: &mut kn,
                            world: &f.world_a,
                            model,
                            now,
                        },
                        ev,
                        &d,
                    ));
                }
                if ev.side != Side::A {
                    let d = f.world_b.apply(op);
                    if ev.side == Side::Both {
                        kn.set_view(View::B, View::B.bit());
                    }
                    record(&mut kn, &d);
                    all.extend(scheme.apply(
                        &mut Cx {
                            kn: &mut kn,
                            world: &f.world_b,
                            model,
                            now,
                        },
                        ev,
                        &d,
                    ));
                }
            }
            (op, None) => {
                let d = world.apply(op);
                kn.set_view(View::Main, ALL_VIEWS);
                record(&mut kn, &d);
                all.extend(scheme.apply(
                    &mut Cx {
                        kn: &mut kn,
                        world: &world,
                        model,
                        now,
                    },
                    ev,
                    &d,
                ));
            }
        }
    }
    kn.set_view(View::Main, ALL_VIEWS);
    all.extend(scheme.finish(&mut Cx {
        kn: &mut kn,
        world: &world,
        model,
        now: trace.end_ms,
    }));
    let invariants = kn.check(opts.check);

    let mut classes: BTreeMap<(CostClass, bool), ClassStats> = BTreeMap::new();
    let mut xw = 0u64;
    let mut sg = 0u64;
    let mut max_op = Cpu::default();
    let mut plain = 0u64;
    for c in setup.iter().chain(&all) {
        classes
            .entry((c.class, c.group == CREW))
            .or_insert_with(|| ClassStats::new(c.class, c.group == CREW))
            .add(c);
    }
    for c in &all {
        xw += c.xwing;
        sg += c.signatures;
        let one = Cpu::of(c.xwing, c.signatures);
        if one.seconds > max_op.seconds {
            max_op = one;
        }
        plain += if c.group == CREW { c.own } else { c.others };
    }
    let mut classes: Vec<ClassStats> = classes.into_values().collect();
    for c in &mut classes {
        c.finish();
    }
    KeyRun {
        scheme: label,
        devices: trace.initial_devices(),
        users: trace.initial.len(),
        days: trace.churn.days,
        ops: trace.events.len(),
        classes,
        invariants,
        notes: scheme.notes(),
        cpu: Cpu::of(xw, sg),
        cpu_max_op: max_op,
        plain_device_per_day: plain as f64 / trace.churn.days.max(1e-9),
        elapsed_ms: started.elapsed().as_millis() as u64,
    }
}

// ───────────── 묶음 창(옛 MLS·키 트리가 같이 씀) ─────────────

/// 한 그룹에 쌓인 변화.
#[derive(Clone, Debug, Default)]
pub(crate) struct PendingGroup {
    pub add: BTreeSet<Dev>,
    pub remove: BTreeSet<Dev>,
    pub causes: BTreeSet<CostClass>,
}

impl PendingGroup {
    /// 섞인 원인 중 대표(제거가 가입보다 앞).
    pub fn class(&self) -> CostClass {
        self.causes
            .iter()
            .copied()
            .max_by_key(|c| c.rank())
            .unwrap_or(CostClass::Merge)
    }
}

/// 확정 기기의 묶음 창: 첫 변화부터 `window_ms` 뒤에 냅니다. `extend`면 제거가 이어지는 동안 첫 제거부터
/// `window_max_ms`까지 늘립니다.
#[derive(Clone, Debug, Default)]
pub(crate) struct Pending {
    pub groups: BTreeMap<Group, PendingGroup>,
    first_removal: Option<u64>,
    deadline: Option<u64>,
}

impl Pending {
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    pub fn due(&self, now: u64) -> Option<u64> {
        self.deadline.filter(|d| *d <= now)
    }

    /// 변화를 쌓습니다. 같은 창 안에서 더했다 뺀 기기는 지웁니다.
    pub fn push(&mut self, d: &Delta, op: &Op, now: u64, m: &CostModel, extend: bool) {
        let mut removal = false;
        for &(g, dev) in &d.added {
            let pg = self.groups.entry(g).or_default();
            if !pg.remove.remove(&dev) {
                pg.add.insert(dev);
            }
            pg.causes.insert(cause_of(op, true));
        }
        for &(g, dev) in &d.removed {
            let pg = self.groups.entry(g).or_default();
            if !pg.add.remove(&dev) {
                pg.remove.insert(dev);
            }
            pg.causes.insert(cause_of(op, false));
            removal = true;
        }
        self.groups
            .retain(|_, pg| !(pg.add.is_empty() && pg.remove.is_empty()));
        if self.groups.is_empty() {
            self.deadline = None;
            self.first_removal = None;
            return;
        }
        if d.added.is_empty() && d.removed.is_empty() {
            return;
        }
        let base = now + m.window_ms;
        self.deadline = Some(match (self.deadline, removal && extend) {
            (None, _) => base,
            (Some(dl), true) => {
                let first = *self.first_removal.get_or_insert(now);
                dl.max(base.min(first + m.window_max_ms))
            }
            (Some(dl), false) => dl,
        });
        if removal {
            self.first_removal.get_or_insert(now);
        }
    }

    pub fn take(&mut self) -> BTreeMap<Group, PendingGroup> {
        self.deadline = None;
        self.first_removal = None;
        std::mem::take(&mut self.groups)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scale::trace::{Churn, Handover, UserInit, generate};

    /// 봉인 상자 `n`바이트(무작위)를 실은 실제 키 갱신 이벤트(상태 키로 암호화, 부모 2개)의 평문 본문 길이와 전송 크기.
    fn real_event_len(n: usize) -> (usize, usize) {
        use yangpa_core::body::{EventBody, Secret32};
        use yangpa_core::event::{EVENT_VERSION, EventHeader, KeyRef, SignedEvent};
        use yangpa_core::ids::{DeviceId, EventId, KeyGroupId, SpaceId, StateKeyId, UserId};
        use yangpa_core::keyupdate::{KeyUpdateKind, KeyUpdatePayload, RCt, SealedBox};
        use yangpa_core::time::Timestamp;
        let keys = yangpa_crypto::DeviceKeys::generate();
        let space = SpaceId([1; 16]);
        let sealed: Vec<u8> = (0..n).map(|_| rand::random()).collect();
        let body = EventBody::KeyUpdate(KeyUpdatePayload {
            group: KeyGroupId::crew(&space),
            seq: 7,
            prev: Some(EventId([5; 32])),
            kind: KeyUpdateKind::Rotate {
                removed: vec![DeviceId([6; 32])],
                joined: Vec::new(),
            },
            root_commit: [7; 32],
            prev_root_commit: [8; 32],
            crew: None,
            bundle_root: None,
            shards: None,
            depth: 0,
            salt: [9; 24],
            layout: Vec::new(),
            wraps: Vec::new(),
            sealed: vec![SealedBox(sealed)],
            r_ct: Some(RCt { ct: [10; 48] }),
            part: 0,
            parts: 1,
        });
        let header = EventHeader {
            v: EVENT_VERSION,
            space,
            channel: None,
            kind: body.kind(),
            author: keys.device_id(),
            user: UserId([2; 32]),
            parents: vec![EventId([3; 32]), EventId([4; 32])],
            lamport: 100_000,
            created_at: Timestamp(1_800_000_000_000),
            key: KeyRef::State(StateKeyId([11; 16])),
            body_hash: [0; 32],
            body_len: 0,
            scope: None,
            state_anchor: Vec::new(),
        };
        let plain = body.encode().unwrap().len();
        let ct = yangpa_crypto::encrypt_body(&Secret32::random(), &header, &body).unwrap();
        let ev = SignedEvent::sign(header, ct, &keys.signing).unwrap();
        (plain, postcard::to_stdvec(&ev).unwrap().len())
    }

    /// 이벤트 크기 모형이 실제 이벤트와 맞는지: 평문 본문 길이로 센 모형이 실제(압축 표시·Padmé·태그 포함)와 1% 안
    /// 아래, Padmé 덧붙임(12%) 안 위.
    #[test]
    fn event_size_matches_real_event() {
        let m = CostModel::default();
        for n in [1_000usize, 30_000, 200_000] {
            let (plain, real) = real_event_len(n);
            let (real, model) = (real as f64, m.event(plain) as f64);
            assert!(
                model >= real * 0.99 && model <= real * 1.12,
                "본문 {plain}: 실제 {real}, 모형 {model}"
            );
        }
    }

    fn small_trace(days: f64) -> Trace {
        let mut c = Churn::new(300, days, 11);
        c.joins_per_day = 40.0;
        c.removals_per_day = 40.0;
        c.device_adds_per_day = 5.0;
        c.revocations_per_day = 5.0;
        c.flips_per_day = 10.0;
        c.mass_ban = 30;
        generate(&c)
    }

    fn tree_with(mutant: tree::Mutant) -> Box<dyn KeyScheme + Send> {
        let mut m = KeyTreeModel::default();
        m.mutant = mutant;
        Box::new(m)
    }

    fn run_tree(t: &Trace, mutant: tree::Mutant) -> KeyRun {
        run_scheme(
            tree_with(mutant),
            "망가뜨린 키 트리",
            t,
            &CostModel::default(),
            RunOpts::default(),
        )
    }

    /// 300대·2일 기록(확정 기기 폐기·강등·잃음·넘김, 넘겨준 옛 확정 기기의 강등·폐기, 예비 기기 교체·폐기, 갈라짐,
    /// 밴 물결 포함)에서 키 트리 모형은 위반이 없습니다. 표 보유 기기가 빠질 때 전체 재키를 빼거나 제거 회전을
    /// 빼면 검사기가 잡습니다.
    #[test]
    fn key_tree_model_small_trace_no_violations() {
        let t = small_trace(2.0);
        let counts = t.counts();
        assert!(
            counts["강등"] >= 2 && counts["예비 기기"] >= 2,
            "{counts:?}"
        );
        assert!(
            t.events.iter().any(|e| matches!(
                e.op,
                Op::DeviceRevoke {
                    repair: Some(_),
                    ..
                }
            )),
            "확정 기기 폐기 없음"
        );
        let r = run(
            SchemeKind::KeyTree,
            &t,
            &CostModel::default(),
            RunOpts::default(),
        );
        assert!(r.invariants.tracked);
        assert_eq!(r.invariants.checked, r.invariants.devices);
        assert_eq!(r.invariants.violations(), 0, "{:#?}", r.invariants);
        // 확정 기기 강등·폐기·잃음, 갈라짐(B쪽 잃음), 옛 확정 기기 강등·폐기, 예비 기기 교체·폐기 = 8번.
        let full = r.class(CostClass::FullRekey, true).expect("전체 재키");
        assert!(
            full.count >= 8,
            "표 보유 기기가 빠질 때마다 전체 재키: {}",
            full.count
        );
        let rm = r.class(CostClass::Removal, true).expect("탈퇴");
        assert!(rm.mean(|c| c.own) > 1_000.0);

        let no_full = run_tree(&t, tree::Mutant::NoHolderRekey);
        assert!(no_full.invariants.i2 > 0, "{:#?}", no_full.invariants);
        let no_rot = run_tree(&t, tree::Mutant::NoRemovalRotation);
        assert!(no_rot.invariants.i2_stale > 0, "{:#?}", no_rot.invariants);
    }

    /// 옛 MLS도 같은 기록에서 위반이 없습니다(경로 있는 커밋으로만 제거). 2일 기록이라 새 확정 기기가 가입 기기인
    /// 커밋(비공개 그룹의 유일한 기기가 옛 확정 기기)도 지납니다.
    #[test]
    fn legacy_mls_small_trace_no_key_violations() {
        let t = small_trace(2.0);
        let r = run(
            SchemeKind::LegacyMls,
            &t,
            &CostModel::default(),
            RunOpts::default(),
        );
        let i = &r.invariants;
        assert_eq!((i.i1, i.i2, i.i3), (0, 0, 0), "{i:#?}");
    }

    /// 사용자 `users`명(기기 2대씩, 사용자 u의 기기는 2u·2u+1). 0이 소유자, 1..=admins가 관리자.
    fn crew_init(users: u32, admins: u32, groups: impl Fn(u32) -> Vec<Group>) -> Vec<UserInit> {
        (0..users)
            .map(|u| UserInit {
                user: u,
                devices: vec![2 * u, 2 * u + 1],
                groups: groups(u),
                admin: u >= 1 && u <= admins,
            })
            .collect()
    }

    fn custom(initial: Vec<UserInit>, groups: usize, events: Vec<(u64, Op)>) -> Trace {
        let devices = initial.iter().map(|u| u.devices.len()).sum();
        Trace {
            churn: Churn::new(devices, 1.0, 1),
            initial,
            owner: 0,
            committer: 0,
            groups,
            events: events
                .into_iter()
                .map(|(at_ms, op)| Event {
                    at_ms,
                    side: Side::Both,
                    op,
                })
                .collect(),
            end_ms: 60_000,
        }
    }

    /// 비공개 그룹이 처음부터 비었다가 기기를 얻거나, 비었다가 다시 찰 때: 옛 T에서 전진하면 떠난 기기가 계산하고
    /// (I2), 키가 없는 그룹에서 전진하면 터집니다. (생성기 기록은 소유자·관리자가 모든 그룹에 들어 비지 않지만 모형은
    /// 버텨야 합니다.)
    #[test]
    fn empty_private_groups_get_fresh_keys() {
        // 그룹 1: 사용자 5만. 그룹 2: 아무도 없음.
        let initial = crew_init(20, 2, |u| if u == 5 { vec![1] } else { Vec::new() });
        let flip = |user, group, gain| Op::GroupFlip { user, group, gain };
        let t = custom(
            initial,
            2,
            vec![
                (1_000, flip(7, 2, true)),
                (2_000, flip(5, 1, false)),
                (5_000, flip(6, 1, true)),
                (
                    8_000,
                    Op::Join {
                        user: 100,
                        devices: vec![1_000],
                        groups: vec![1, 2],
                    },
                ),
                (14_000, flip(8, 2, true)),
                (17_000, flip(6, 1, false)),
                (20_000, flip(9, 1, true)),
            ],
        );
        for kind in SchemeKind::ALL {
            let r = run(kind, &t, &CostModel::default(), RunOpts::default());
            assert_eq!(
                r.invariants.violations(),
                0,
                "{kind:?}: {:#?}",
                r.invariants
            );
        }
    }

    /// 지금 MLS: 새 확정 기기가 아직 멤버가 아닌 그룹(유일한 MLS 멤버가 옛 확정 기기)의 커밋은 새 기기에 웰컴 봉인
    /// 하나뿐입니다(전에는 봉인 수를 n − 1 − 가입 수로 세어 디버그 빌드에서 뺄셈이 넘쳤음).
    #[test]
    fn legacy_mls_committer_joining_private_group() {
        let initial = crew_init(20, 2, |u| if u == 5 { vec![1] } else { Vec::new() });
        let t = custom(
            initial,
            2,
            vec![(
                1_000,
                Op::Committer {
                    to: 2,
                    how: Handover::Voluntary,
                },
            )],
        );
        let r = run(
            SchemeKind::LegacyMls,
            &t,
            &CostModel::default(),
            RunOpts::default(),
        );
        assert_eq!(r.invariants.violations(), 0, "{:#?}", r.invariants);
        // 그룹 1: 사용자 5의 기기 2대에 경로 + 새 확정 기기 웰컴 = 3, 그룹 2: 새 확정 기기 웰컴 = 1.
        let c = r
            .class(CostClass::Committer, false)
            .expect("확정 기기 변경");
        assert_eq!((c.count, c.sum.xwing), (2, 4), "{c:?}");
    }

    /// 넘겨준 옛 확정 기기(표 보유 기기)가 강등·폐기되거나 비공개 그룹 자격을 잃을 때, 예비 기기가 바뀔 때 키 트리는
    /// 전체 재키합니다. 빼면(망가뜨린 모형) 옛 leaf_master로 새 잎·경로를 계산해 I2로 잡힙니다.
    #[test]
    fn key_tree_rekeys_when_former_committer_or_backup_leaves() {
        // 기기 400대(트리 인코딩), 모두 그룹 1. 관리자 1·2(기기 2·4)에게 차례로 넘긴 뒤 관리자 1을 뺌.
        let initial = crew_init(200, 4, |_| vec![1]);
        let handovers = |extra: (u64, Op)| {
            let mut v = vec![
                (500, Op::Backup { device: Some(1) }),
                (
                    1_000,
                    Op::Committer {
                        to: 2,
                        how: Handover::Voluntary,
                    },
                ),
                (
                    2_000,
                    Op::Committer {
                        to: 4,
                        how: Handover::Voluntary,
                    },
                ),
                extra,
                (
                    4_000,
                    Op::Join {
                        user: 300,
                        devices: vec![1_000, 1_001],
                        groups: vec![1],
                    },
                ),
            ];
            v.sort_by_key(|e| e.0);
            v
        };
        let cases = [
            (
                "강등",
                Op::Demote {
                    user: 1,
                    to: 4,
                    keep: Vec::new(),
                },
            ),
            (
                "폐기",
                Op::DeviceRevoke {
                    user: 1,
                    device: 2,
                    repair: None,
                },
            ),
            ("탈퇴", Op::Leave { user: 1 }),
            (
                "그룹 자격 잃음",
                Op::GroupFlip {
                    user: 1,
                    group: 1,
                    gain: false,
                },
            ),
            ("예비 기기 교체", Op::Backup { device: Some(3) }),
        ];
        for (name, op) in cases {
            let t = custom(initial.clone(), 1, handovers((3_000, op)));
            let r = run(
                SchemeKind::KeyTree,
                &t,
                &CostModel::default(),
                RunOpts::default(),
            );
            assert_eq!(r.invariants.violations(), 0, "{name}: {:#?}", r.invariants);
            let full = r.class(CostClass::FullRekey, true).map_or(0, |c| c.count);
            assert_eq!(full, 1, "{name}: 전체 재키 {full}번");
            let bad = run_tree(&t, tree::Mutant::NoHolderRekey);
            assert!(bad.invariants.i2 > 0, "{name}: {:#?}", bad.invariants);
        }
    }

    /// 원안은 제거 회전 전에 가입 전진을 내므로 제거된 기기가 전진한 크루 키를 계산합니다(검사기가 잡는지).
    #[test]
    fn brief_model_flags_advance_before_rotation() {
        let initial: Vec<UserInit> = (0..20u32)
            .map(|u| UserInit {
                user: u,
                devices: vec![2 * u, 2 * u + 1],
                groups: Vec::new(),
                admin: u == 1,
            })
            .collect();
        let events = vec![
            Event {
                at_ms: 1_000,
                side: Side::Both,
                op: Op::Leave { user: 7 },
            },
            Event {
                at_ms: 1_100,
                side: Side::Both,
                op: Op::Join {
                    user: 100,
                    devices: vec![1_000],
                    groups: Vec::new(),
                },
            },
            Event {
                at_ms: 5_000,
                side: Side::Both,
                op: Op::Committer {
                    to: 2,
                    how: Handover::Voluntary,
                },
            },
        ];
        let t = Trace {
            churn: Churn::new(40, 1.0, 1),
            initial,
            owner: 0,
            committer: 0,
            groups: 0,
            events,
            end_ms: 10_000,
        };
        let brief = run(
            SchemeKind::BriefSubgroups,
            &t,
            &CostModel::default(),
            RunOpts::default(),
        );
        assert!(brief.invariants.i2 >= 1, "{:#?}", brief.invariants);
        let tree = run(
            SchemeKind::KeyTree,
            &t,
            &CostModel::default(),
            RunOpts::default(),
        );
        assert_eq!(tree.invariants.violations(), 0, "{:#?}", tree.invariants);
    }

    /// 밴 물결만 있는 기록(가입 없음)에서는 어느 방식도 위반이 없습니다. 한 이벤트로 여럿을 내보낼 때 먼저 낸 하위
    /// 그룹 커밋이 곧 빠질 기기에게 새 에포크를 주면 잡힙니다. 밴 뒤 회전하지 않는 모형은 낡은 키(I2)로 잡힙니다.
    #[test]
    fn ban_wave_alone_has_no_violations() {
        let initial: Vec<UserInit> = (0..20u32)
            .map(|u| UserInit {
                user: u,
                devices: vec![2 * u, 2 * u + 1],
                groups: if u % 2 == 0 { vec![1] } else { Vec::new() },
                admin: u == 1,
            })
            .collect();
        let ban = |at_ms, users| Event {
            at_ms,
            side: Side::Both,
            op: Op::Ban { users },
        };
        let t = Trace {
            churn: Churn::new(40, 1.0, 1),
            initial,
            owner: 0,
            committer: 0,
            groups: 1,
            events: vec![
                ban(1_000, vec![3, 4, 5]),
                ban(1_700, vec![6, 7]),
                ban(2_400, vec![8]),
            ],
            end_ms: 10_000,
        };
        for kind in SchemeKind::ALL {
            let r = run(kind, &t, &CostModel::default(), RunOpts::default());
            assert_eq!(
                r.invariants.violations(),
                0,
                "{kind:?}: {:#?}",
                r.invariants
            );
        }
        let bad = run_tree(&t, tree::Mutant::NoRemovalRotation);
        assert!(bad.invariants.i2_stale > 0, "{:#?}", bad.invariants);
    }
}
