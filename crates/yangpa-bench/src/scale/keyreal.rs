//! 실제 키 트리 재기(`yangpa-bench scale --keytree-real 45053,200`).
//!
//! [`yangpa_crypto::keytree`]의 확정 기기 코드를 큰 크루에서 그대로 돌려 `KeyUpdate` 인코딩 크기·조각 수·봉인 묶음
//! 크기·확정 기기 CPU(벽시계, 한 스레드)와 멤버 하나의 적용 시간을 잽니다. 같은 사용자·기기·대상으로 만든 기록을 키
//! 트리 모형([`super::keys::KeyTreeModel`])에도 돌려 나란히 놓습니다. 모형을 실제에 맞추려고 고치지 않습니다.
//!
//! - 이벤트 크기는 모형과 같은 식(고정 1,909B + Padmé(4 + 본문))입니다. 서명은 하지 않으므로 확정 기기 CPU에는
//!   이벤트마다 서명 어림([`EVENT_SIGN_SECS`])을 더합니다. 발행할 때마다 첫 조각의 이벤트 ID(조각 바이트의 해시)를
//!   [`KeyTree::set_last_event`]로 알려 다음 갱신의 `prev`가 실제처럼 실립니다.
//! - 봉인 묶음 크기는 전송 인코딩([`DeviceBundle::encode`]: 봉투 + 기기 ID + 머클 자리·증명) 그대로 셉니다.
//! - 상자 키는 키 쌍 16개를 기기들이 나눠 씁니다(봉인은 기기 ID로 나뉘고 캡슐화 비용은 같음).
//! - 작업 순서: 처음 구성(직접 → 트리는 전체 재키) → 기기 1대 사용자 탈퇴 → 기기 1대 가입 → 밴 물결 →
//!   시간 회전 → 넘겨받지 못한 확정 기기의 전체 재키(공개 끝에서 [`KeyTree::recover`]).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use rand::SeedableRng;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use serde::Serialize;
use yangpa_core::device::BoxPublicKey;
use yangpa_core::ids::{DeviceId, EventId, KeyGroupId, SpaceId, UserId};
use yangpa_core::keyupdate::{KeyUpdatePayload, MAX_PAYLOAD_LEN, PART_TARGET_LEN};
use yangpa_crypto::keytree::{
    ChainTip, CommitCtx, CoverGroup, DeviceBundle, GroupKind, KeyTree, KeyUpdateOutput, MemberCtx,
    MemberKeys, by_user, encode_table,
};
use yangpa_crypto::sealed::BoxKeypair;

use super::keys::{
    self, ClassStats, CostClass, CostModel, EVENT_SIGN_SECS, KeyRun, RunOpts, SEALED_BOX_8_SECS,
    SchemeKind,
};
use super::trace::{Churn, DAY_MS, Dev, Event, Handover, Op, Side, Trace, User, UserInit};

/// 한 작업의 실제·모형 비교.
#[derive(Clone, Debug, Serialize)]
pub struct RealRow {
    pub op: String,
    /// 실제 바이트(무엇을 셌는지는 `op`).
    pub real: f64,
    pub model: Option<f64>,
    /// 목표(십진 바이트).
    pub target: Option<f64>,
    /// 확정 기기 CPU 초: 실제(키 계산 벽시계 + 서명 어림), 모형(X-Wing·서명 어림).
    pub real_cpu: Option<f64>,
    pub model_cpu: Option<f64>,
    /// 확정 기기 CPU 목표(초).
    pub cpu_target: Option<f64>,
    /// 멤버 기기 하나가 합친 페이로드를 적용하는 시간(초).
    pub member_secs: Option<f64>,
    pub detail: String,
}

impl RealRow {
    /// 모형 대비 차이(%).
    pub fn diff(&self) -> Option<f64> {
        self.model
            .filter(|m| *m > 0.0)
            .map(|m| (self.real - m) / m * 100.0)
    }

    /// 목표를 지켰는지(바이트·CPU 목표가 모두 없으면 `None`).
    pub fn meets(&self) -> Option<bool> {
        let bytes = self.target.map(|t| self.real <= t);
        let cpu = self.cpu_target.zip(self.real_cpu).map(|(t, c)| c <= t);
        match (bytes, cpu) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(true) && b.unwrap_or(true)),
        }
    }
}

/// 한 크루 크기의 결과.
#[derive(Clone, Debug, Serialize)]
pub struct RealRun {
    pub devices: usize,
    pub users: usize,
    pub banned_devices: usize,
    /// 처음 구성 뒤 트리 깊이(직접이면 0).
    pub depth: u8,
    pub setup_secs: f64,
    pub model_secs: f64,
    pub rows: Vec<RealRow>,
}

/// 기기 1~2대짜리 사용자들(홀수 번째 사용자가 2대, 평균 1.5대).
struct Roster {
    users: Vec<Vec<usize>>,
    user_of: Vec<User>,
    ids: Vec<DeviceId>,
}

fn device_id(seed: u64, i: usize) -> DeviceId {
    let mut h = blake3::Hasher::new_derive_key("yangpa-bench/keyreal device");
    h.update(&seed.to_le_bytes());
    h.update(&(i as u64).to_le_bytes());
    DeviceId(*h.finalize().as_bytes())
}

fn user_id(u: User) -> UserId {
    let mut b = [0u8; UserId::LEN];
    b[..4].copy_from_slice(&u.to_be_bytes());
    UserId(b)
}

impl Roster {
    /// `n`대(가입자 1대 몫 ID까지 미리 만듦).
    fn new(n: usize, seed: u64) -> Self {
        let mut users = Vec::new();
        let mut user_of = Vec::with_capacity(n + 1);
        let mut i = 0;
        while i < n {
            let k = if users.len() % 2 == 1 { 2 } else { 1 }.min(n - i);
            user_of.extend(std::iter::repeat_n(users.len() as User, k));
            users.push((i..i + k).collect());
            i += k;
        }
        user_of.push(users.len() as User);
        let ids = (0..=n).map(|i| device_id(seed, i)).collect();
        Roster {
            users,
            user_of,
            ids,
        }
    }
}

/// 모형과 실제가 함께 쓰는 대상.
struct Plan {
    leaver: User,
    joiner_user: User,
    joiner_dev: usize,
    banned: Vec<User>,
    banned_devs: Vec<usize>,
    new_committer: usize,
}

fn plan(r: &Roster, ban: usize, seed: u64) -> Result<Plan> {
    ensure!(r.users.len() >= 6, "사용자가 너무 적음(기기 8대 이상)");
    // 기기 1대 사용자(짝수 번째) 중 확정 기기 사용자가 아닌 첫 사용자.
    let leaver = 2;
    let mut cand: Vec<User> = (3..r.users.len() as User).collect();
    cand.shuffle(&mut StdRng::seed_from_u64(seed));
    let (mut banned, mut banned_devs) = (Vec::new(), Vec::new());
    for u in cand {
        if banned_devs.len() >= ban {
            break;
        }
        banned.push(u);
        banned_devs.extend(r.users[u as usize].iter().copied());
    }
    Ok(Plan {
        leaver,
        joiner_user: r.users.len() as User,
        joiner_dev: r.ids.len() - 1,
        banned,
        banned_devs,
        new_committer: r.users[1][0],
    })
}

/// 같은 작업을 모형에 돌릴 기록.
fn model_trace(r: &Roster, p: &Plan, seed: u64) -> Trace {
    let n = r.ids.len() - 1;
    let end_ms = DAY_MS + 180_000;
    let mut churn = Churn::new(n, end_ms as f64 / DAY_MS as f64, seed);
    churn.private_groups = Vec::new();
    churn.mass_ban = p.banned.len();
    let initial = r
        .users
        .iter()
        .enumerate()
        .map(|(u, devs)| UserInit {
            user: u as User,
            devices: devs.iter().map(|&d| d as Dev).collect(),
            groups: Vec::new(),
            admin: u < 2,
        })
        .collect();
    let ev = |at_ms: u64, op: Op| Event {
        at_ms,
        side: Side::Both,
        op,
    };
    Trace {
        churn,
        initial,
        owner: 0,
        committer: r.users[0][0] as Dev,
        groups: 0,
        events: vec![
            ev(1_000, Op::Leave { user: p.leaver }),
            ev(
                10_000,
                Op::Join {
                    user: p.joiner_user,
                    devices: vec![p.joiner_dev as Dev],
                    groups: Vec::new(),
                },
            ),
            ev(
                20_000,
                Op::Ban {
                    users: p.banned.clone(),
                },
            ),
            // 모형은 다음 사건 때 묶음 창을 닫으므로 아무것도 바꾸지 않는 사건으로 밴을 바로 냅니다(시간 회전 기준
            // 시각이 밴 직후가 됨). 하루 뒤 같은 사건에서 시간 회전이 돌고, 확정 기기를 넘겨받지 못함 → 전체 재키.
            ev(30_000, Op::Ownership { to: 0 }),
            ev(DAY_MS + 60_000, Op::Ownership { to: 0 }),
            ev(
                DAY_MS + 120_000,
                Op::Committer {
                    to: p.new_committer as Dev,
                    how: Handover::Lost,
                },
            ),
        ],
        end_ms,
    }
}

/// 실제 갱신 하나: 나누고 크기를 셉니다.
struct Measured {
    parts: usize,
    /// 조각 이벤트 크기 합.
    published: f64,
    largest_part: usize,
    body: usize,
    wraps: usize,
    secs: f64,
    /// 첫 조각의 이벤트 ID 대신(다음 갱신의 `prev`).
    first: EventId,
}

fn measure(out: &KeyUpdateOutput, secs: f64, m: &CostModel) -> Result<Measured> {
    let parts = out.payload.clone().split(PART_TARGET_LEN)?;
    let mut published = 0.0;
    let mut largest = 0;
    for part in &parts {
        let len = part.encoded_len();
        ensure!(len <= MAX_PAYLOAD_LEN, "조각이 한도를 넘음: {len}B");
        largest = largest.max(len);
        published += m.event(len) as f64;
    }
    // 실제로 받는 쪽이 하는 대로 디코딩해 합쳐 봅니다.
    let decoded = parts
        .iter()
        .map(|p| KeyUpdatePayload::decode(&p.encode()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        KeyUpdatePayload::merge(decoded)? == out.payload,
        "합친 조각이 원래와 다름"
    );
    Ok(Measured {
        first: EventId::hash_of(&parts[0].encode()),
        parts: parts.len(),
        published,
        largest_part: largest,
        body: out.payload.encoded_len(),
        wraps: out.payload.wraps.len(),
        secs: secs + parts.len() as f64 * EVENT_SIGN_SECS,
    })
}

/// 저장 칸 하나·넘기기 요청 하나에 드는 그룹 상태의 상한: 메시지 상한(8MiB)에서 넘기기 봉투·메타데이터 여유를 뺀 값
/// (데몬 `seal_part`와 같음). 저장 칸 하나의 상한(16MiB)보다 좁습니다.
const GROUP_ROW_LIMIT: f64 = (8 * 1024 * 1024 - 64 * 1024) as f64;

/// 확정 기기 표의 그룹마다 크기(저장 칸 하나·넘기기 요청 하나): 크루, 크루의 90%를 담은 비공개 트리, 32대만 뺀 덮개.
/// 예전에는 표 전체를 한 행·한 요청에 담아, 크루 + 큰 비공개 그룹 3 + 덮개 4가 저장 상한(16MiB)을 넘었습니다.
fn table_rows(
    space: SpaceId,
    crew: &KeyTree,
    all: &[DeviceId],
    boxes: &BTreeMap<DeviceId, BoxPublicKey>,
) -> Result<Vec<RealRow>> {
    let cx = CommitCtx {
        boxes,
        crew: Some(crew),
    };
    let crew_len = encode_table(&[crew], &[]).len();
    let mut private = KeyTree::private(space, KeyGroupId::private(&space, b"bench-private"));
    let k = all.len() * 9 / 10;
    let first = k.min(300);
    let t = Instant::now();
    private
        .update(&[], &all[..first], &cx)
        .context("비공개 만들기")?;
    if k > first {
        private
            .update(&[], &all[first..k], &cx)
            .context("비공개 가입")?;
    }
    let private_secs = t.elapsed().as_secs_f64();
    let private_len = encode_table(&[&private], &[]).len();
    let mut cover = CoverGroup::new(space, KeyGroupId::private(&space, b"bench-cover"));
    let step = (all.len() / 32).max(1);
    let excluded: BTreeSet<DeviceId> = all.iter().step_by(step).take(32).copied().collect();
    cover.update(&excluded, &cx).context("덮개")?;
    let cover_len = encode_table(&[], &[&cover]).len();
    let total = crew_len + 3 * private_len + 4 * cover_len;
    let row = |op: String, real: usize, target: Option<f64>, detail: String| RealRow {
        op,
        real: real as f64,
        model: None,
        target,
        real_cpu: None,
        model_cpu: None,
        cpu_target: None,
        member_secs: None,
        detail,
    };
    Ok(vec![
        row(
            "확정 기기 표: 크루 칸".into(),
            crew_len,
            Some(GROUP_ROW_LIMIT),
            format!(
                "기기 {}대, 깊이 {}(잎은 leaf_master에서 다시 만듦)",
                all.len(),
                crew.depth()
            ),
        ),
        row(
            "확정 기기 표: 비공개 그룹 칸(크루의 90%)".into(),
            private_len,
            Some(GROUP_ROW_LIMIT),
            format!("기기 {k}대, 만들기 {private_secs:.2}초"),
        ),
        row(
            "확정 기기 표: 덮개 그룹 칸(32대 뺌)".into(),
            cover_len,
            Some(GROUP_ROW_LIMIT),
            format!("멤버 {}대", cover.members().len()),
        ),
        row(
            "확정 기기 표: 크루 + 비공개 3 + 덮개 4 합".into(),
            total,
            None,
            "그룹마다 칸·요청이 따로라 합에는 상한 없음(예전 한 행 상한 16MiB)".into(),
        ),
    ])
}

fn model_cpu(c: &ClassStats) -> f64 {
    c.mean(|o| o.xwing) * SEALED_BOX_8_SECS / 8.0 + c.mean(|o| o.signatures) * EVENT_SIGN_SECS
}

fn note(r: &KeyRun, key: &str) -> Option<f64> {
    r.notes
        .iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| v.trim_end_matches('B').parse().ok())
}

/// 크루 `n`대에서 실제 키 트리와 모형을 돌립니다. `ban`은 밴 물결로 뺄 기기 수(사용자 단위로 채움).
pub fn run(n: usize, ban: usize, seed: u64, m: &CostModel) -> Result<RealRun> {
    let roster = Roster::new(n, seed);
    let ban = ban.min(n / 5);
    let p = plan(&roster, ban, seed)?;

    // ---- 모형 ----
    let t = Instant::now();
    let trace = model_trace(&roster, &p, seed);
    let opts = RunOpts {
        track: false,
        ..RunOpts::default()
    };
    let mr = keys::run(SchemeKind::KeyTree, &trace, m, opts);
    let model_secs = t.elapsed().as_secs_f64();
    let class = |c: CostClass| mr.class(c, true);

    // ---- 실제 ----
    let mut sb = [0u8; SpaceId::LEN];
    sb.copy_from_slice(&blake3::hash(&seed.to_le_bytes()).as_bytes()[..SpaceId::LEN]);
    let space = SpaceId(sb);
    let pool: Vec<BoxKeypair> = (0..16).map(|_| BoxKeypair::generate()).collect();
    let kp = |i: usize| &pool[i % pool.len()];
    let boxes: BTreeMap<DeviceId, BoxPublicKey> = roster
        .ids
        .iter()
        .enumerate()
        .map(|(i, d)| (*d, kp(i).public()))
        .collect();
    let cx = CommitCtx {
        boxes: &boxes,
        crew: None,
    };
    let all: Vec<DeviceId> = roster.ids[..n].to_vec();
    let mut crew = KeyTree::crew(space);
    let t = Instant::now();
    let setup = crew.update(&[], &all, &cx).context("처음 구성")?;
    let setup_secs = t.elapsed().as_secs_f64();
    crew.set_last_event(measure(&setup, setup_secs, m)?.first);
    let depth = crew.depth();

    // 멤버 하나(새 확정 기기가 될 사용자 1의 기기)를 따라가며 적용 시간을 잽니다.
    let me = p.new_committer;
    let bundle_of = |out: &KeyUpdateOutput, i: usize| {
        out.bundles
            .iter()
            .find(|b| b.device == roster.ids[i])
            .map(|b| DeviceBundle::decode(&b.encode()))
            .transpose()
    };
    let b = bundle_of(&setup, me)?;
    let mut mine = MemberKeys::join(
        space,
        *crew.group(),
        GroupKind::Crew,
        roster.ids[me],
        &setup.payload,
        &MemberCtx {
            box_keys: Some(kp(me)),
            bundle: b.as_ref(),
            crew: None,
        },
    )
    .context("멤버 가입")?;
    let apply = |out: &KeyUpdateOutput, mine: &mut MemberKeys| -> Result<f64> {
        let b = bundle_of(out, me)?;
        let t = Instant::now();
        let next = mine.apply(
            &out.payload,
            &MemberCtx {
                box_keys: Some(kp(me)),
                bundle: b.as_ref(),
                crew: None,
            },
        )?;
        let secs = t.elapsed().as_secs_f64();
        *mine = next;
        Ok(secs)
    };

    let tables = table_rows(space, &crew, &all, &boxes)?;
    let mut rows = Vec::new();
    let mode = if depth > 0 { "트리" } else { "직접" };

    // 탈퇴(기기 1대 사용자).
    let leaver = roster.users[p.leaver as usize][0];
    let t = Instant::now();
    let out = crew.update(&[roster.ids[leaver]], &[], &cx)?;
    let mm = measure(&out, t.elapsed().as_secs_f64(), m)?;
    crew.set_last_event(mm.first);
    let member = apply(&out, &mut mine)?;
    let c = class(CostClass::Removal);
    rows.push(RealRow {
        op: format!("탈퇴(기기 1대, {mode}): 크루 기기당 받기"),
        real: mm.published,
        model: c.map(|c| c.mean(|o| o.own)),
        target: (depth > 0).then_some(4_500.0),
        real_cpu: Some(mm.secs),
        model_cpu: c.map(model_cpu),
        cpu_target: None,
        member_secs: Some(member),
        detail: format!("본문 {}B, 감싸기 {}개", mm.body, mm.wraps),
    });

    // 가입(기기 1대).
    let t = Instant::now();
    let out = crew.update(&[], &[roster.ids[p.joiner_dev]], &cx)?;
    let mm = measure(&out, t.elapsed().as_secs_f64(), m)?;
    crew.set_last_event(mm.first);
    let member = apply(&out, &mut mine)?;
    let bundle = out.bundles.first().map_or(0, |b| b.encode().len());
    let c = class(CostClass::Join);
    rows.push(RealRow {
        op: format!("가입(기기 1대, {mode}): 크루 기기당 받기"),
        real: mm.published,
        model: c.map(|c| c.mean(|o| o.own)),
        target: (depth > 0).then_some(2_000.0),
        real_cpu: Some(mm.secs),
        model_cpu: c.map(model_cpu),
        cpu_target: None,
        member_secs: Some(member),
        detail: if depth > 0 {
            format!("본문 {}B", mm.body)
        } else {
            // 직접 인코딩은 가입자 몫 봉인 상자를 이벤트에 싣습니다(모형은 가입자에게만 따로 보냄).
            format!("본문 {}B(가입자 봉인 상자가 이벤트 안)", mm.body)
        },
    });
    rows.push(RealRow {
        op: "가입: 가입자 받기(이벤트 + 봉인 묶음)".into(),
        real: mm.published + bundle as f64,
        model: c.map(|c| c.mean(|o| o.target)),
        target: (depth > 0).then_some(14_000.0),
        real_cpu: None,
        model_cpu: None,
        cpu_target: None,
        member_secs: None,
        detail: if bundle > 0 {
            format!(
                "봉인 묶음 {bundle}B(잎 + 경로 {}칸, 묶음 하나라 증명 없음)",
                crew.depth()
            )
        } else {
            "직접 인코딩: 이벤트 안 봉인 상자".into()
        },
    });

    // 밴 물결.
    let banned: Vec<DeviceId> = p.banned_devs.iter().map(|&i| roster.ids[i]).collect();
    let t = Instant::now();
    let out = crew.update(&banned, &[], &cx)?;
    let mm = measure(&out, t.elapsed().as_secs_f64(), m)?;
    crew.set_last_event(mm.first);
    let member = apply(&out, &mut mine)?;
    let c = class(CostClass::Ban);
    rows.push(RealRow {
        op: format!(
            "밴 물결(기기 {}대·사용자 {}명): 크루 발행 합",
            banned.len(),
            p.banned.len()
        ),
        real: mm.published,
        model: c.map(|c| c.sum.published as f64),
        target: (depth > 0).then_some(1_600_000.0),
        real_cpu: Some(mm.secs),
        model_cpu: c.map(|c| model_cpu(c) * c.count as f64),
        cpu_target: None,
        member_secs: Some(member),
        detail: format!(
            "조각 {}개(가장 큰 조각 {}B), 감싸기 {}개",
            mm.parts, mm.largest_part, mm.wraps
        ),
    });

    // 시간 회전.
    let t = Instant::now();
    let out = crew.refresh_root(&cx)?;
    let mm = measure(&out, t.elapsed().as_secs_f64(), m)?;
    crew.set_last_event(mm.first);
    let member = apply(&out, &mut mine)?;
    let c = class(CostClass::TimeRotation);
    rows.push(RealRow {
        op: format!("시간 회전({mode}): 크루 기기당 받기"),
        real: mm.published,
        model: c.map(|c| c.mean(|o| o.own)),
        target: None,
        real_cpu: Some(mm.secs),
        model_cpu: c.map(model_cpu),
        cpu_target: None,
        member_secs: Some(member),
        detail: format!("본문 {}B, 감싸기 {}개", mm.body, mm.wraps),
    });

    // 넘겨받지 못한 새 확정 기기: 공개 끝에서 이어받아 전체 재키.
    let tip = ChainTip::after(&out.payload, crew.generation());
    let mut fresh = KeyTree::recover(space, *crew.group(), GroupKind::Crew, tip, &crew.devices());
    // 새 확정 기기도 사슬에서 마지막 갱신 이벤트를 압니다.
    fresh.set_last_event(mm.first);
    let t = Instant::now();
    let out = fresh.full_rekey(&[], &[], &cx)?;
    let mm = measure(&out, t.elapsed().as_secs_f64(), m)?;
    let member = apply(&out, &mut mine)?;
    ensure!(mine.t() == fresh.t(), "전체 재키 뒤 멤버 T가 다름");
    let index: BTreeMap<DeviceId, usize> = roster
        .ids
        .iter()
        .enumerate()
        .map(|(i, d)| (*d, i))
        .collect();
    let user_of = |d: &DeviceId| index.get(d).map(|&i| user_id(roster.user_of[i]));
    let bundles_total: usize = out.bundles.iter().map(|b| b.encode().len()).sum();
    let proof_len = out.bundles.first().map_or(0, |b| b.proof.len());
    let per_user = by_user(out.bundles.clone(), user_of);
    let shards: Vec<usize> = per_user
        .values()
        .map(|v| v.iter().map(|b| b.encode().len()).sum())
        .collect();
    let mean_shard = shards.iter().sum::<usize>() as f64 / shards.len().max(1) as f64;
    let max_shard = shards.iter().copied().max().unwrap_or(0);
    let c = class(CostClass::FullRekey);
    let full_cpu = mm.secs;
    // 모형은 사용자 조각마다 블롭 조각 증명(사용자 수의 머클 경로)을 한 번 셉니다. 실제는 기기 몫마다 `bundle_root`
    // 증명(기기 수의 머클 경로)을 싣습니다.
    let proof = 32 * (shards.len().max(2) as f64).log2().ceil() as usize;
    if depth > 0 {
        rows.push(RealRow {
            op: "전체 재키: 사용자 하나의 조각(평균)".into(),
            real: mean_shard,
            model: note(&mr, "전체 재키 블롭/사용자"),
            target: Some(5_000.0),
            real_cpu: None,
            model_cpu: None,
            cpu_target: None,
            member_secs: None,
            detail: format!(
                "기기 몫 {}B(증명 {proof_len}칸 = {}B 포함), 사용자 {}명, 가장 큰 조각 {max_shard}B",
                out.bundles.first().map_or(0, |b| b.encode().len()),
                32 * proof_len,
                shards.len()
            ),
        });
        rows.push(RealRow {
            op: "전체 재키: 기기당 받기(머리 이벤트 + 자기 사용자 조각)".into(),
            real: mm.published + mean_shard,
            model: note(&mr, "전체 재키 기기당 받기"),
            target: Some(5_000.0),
            real_cpu: None,
            model_cpu: None,
            cpu_target: None,
            member_secs: Some(member),
            detail: format!(
                "머리 본문 {}B. 증명은 기기 몫마다 {}B(모형은 사용자 조각마다 약 {proof}B)",
                mm.body,
                32 * proof_len
            ),
        });
    }
    rows.push(RealRow {
        op: format!("전체 재키({mode}): 발행 합(머리 + 모든 조각)"),
        real: mm.published + bundles_total as f64,
        model: c.map(|c| c.sum.published as f64),
        target: None,
        real_cpu: Some(full_cpu),
        model_cpu: c.map(model_cpu),
        cpu_target: Some(90.0),
        member_secs: (depth == 0).then_some(member),
        detail: format!("봉인 {}개", out.bundles.len().max(usize::from(depth == 0))),
    });
    rows.extend(tables);
    Ok(RealRun {
        devices: n,
        users: roster.users.len(),
        banned_devices: banned.len(),
        depth,
        setup_secs,
        model_secs,
        rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 트리 인코딩이 되는 작은 크루로 끝까지 돌고, 모형 값이 모두 있는지.
    #[test]
    fn small_tree_crew_runs_against_the_model() {
        let table = |row: &RealRow| row.op.starts_with("확정 기기 표");
        let r = run(300, 30, 1, &CostModel::default()).expect("실행");
        assert!(r.depth >= 9, "{r:?}");
        assert_eq!(r.rows.len(), 12);
        for row in &r.rows {
            assert!(row.real > 0.0, "{row:?}");
            if table(row) {
                assert_ne!(row.meets(), Some(false), "표 칸이 상한을 넘음: {row:?}");
            } else {
                assert!(row.model.is_some(), "모형 값 없음: {row:?}");
            }
        }
        let d = run(40, 8, 1, &CostModel::default()).expect("직접");
        assert_eq!(d.depth, 0);
        assert!(
            d.rows.iter().all(|row| table(row) || row.model.is_some()),
            "{d:?}"
        );
    }
}
