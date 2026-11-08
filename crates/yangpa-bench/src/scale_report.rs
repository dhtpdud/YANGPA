//! `yangpa-bench scale`의 옵션과 표 찍기(docs/performance.md "규모 시뮬레이션").
//!
//! 옵션 없이 부르면 상태 표만 찍습니다. 새 옵션(`--fanout`, `--concurrent`, `--partition`, `--keys`,
//! `--keytree-real`, `--keytree-daemon`, `--catchup`, `--gossip`, `--store`, `--join-inbox`, `--checkpoint`)만 주면 그
//! 표만 찍고, `--members`를 함께 주면 상태 표도 찍습니다.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use yangpa_bench::scale::{
    self, ScenarioReport, StateReport,
    catchup::{self, CatchupRow, CatchupSpec, StreamCatchup},
    checkpoint::CheckpointReport,
    gossip::{self, GossipRow, HopLatency},
    inbox::{self, JoinInboxRow},
    keydaemon::{self, DaemonOpts, DaemonRun},
    keyreal::{self, RealRun},
    keys::{self, CostClass, CostModel, KeyRun, RunOpts, SchemeKind},
    trace::{self, Churn},
};

/// 기호 추적을 켜는 옛 MLS 모형의 기기 수 상한. 커밋마다 그룹 전체에 새 에포크를 봉인해 추적이 기기 수 × 커밋
/// 수로 커지므로, 그보다 크면 바이트만 셉니다.
const LEGACY_TRACK_MAX: usize = 4_096;

#[derive(clap::Args, Debug)]
pub struct ScaleArgs {
    /// 가입시킬 멤버 수(쉼표로 여럿). 새 옵션 없이 부르면 1000,5000,10000,12000.
    #[arg(long, value_delimiter = ',')]
    members: Option<Vec<usize>>,
    /// 동시 가입 K개(초대 하나를 부모로 둔 가입 기록이 하나씩 도착)를 해소합니다(쉼표로 여럿).
    #[arg(long, value_delimiter = ',')]
    fanout: Vec<usize>,
    /// 가입 중 이 비율(%)이 늦게 온 갈래로 도착할 때의 다시 해소(쉼표로 여럿, 예: 1,5).
    #[arg(long, value_delimiter = ',')]
    concurrent: Vec<f64>,
    /// 두 쪽이 따로 받아 준 뒤 합치기.
    #[arg(long)]
    partition: bool,
    /// `--concurrent`·`--partition`의 크루 멤버 수(멤버 상한 65,536 안).
    #[arg(long, default_value_t = 5_000)]
    crew: usize,
    /// 키 관리 모형을 돌릴 처음 기기 수(쉼표로 여럿, 사용자 = 기기/1.5).
    #[arg(long, value_delimiter = ',')]
    keys: Vec<usize>,
    /// 키 관리 모형의 기록 길이(일).
    #[arg(long, default_value_t = 30.0)]
    days: f64,
    /// 돌릴 키 관리 방식(legacy = 바꾸기 전 MLS 모형, brief = 원안 하위 그룹 모형, tree = 키 그룹 모형).
    #[arg(long, value_delimiter = ',', default_value = "legacy,brief,tree")]
    schemes: Vec<String>,
    /// 실제 키 트리(yangpa-crypto keytree)를 돌릴 크루 기기 수(쉼표로 여럿)와 키 트리 모형 비교.
    #[arg(long, value_delimiter = ',')]
    keytree_real: Vec<usize>,
    /// `--keytree-real`의 밴 물결 기기 수(사용자 단위로 채움, 기기 수의 1/5까지).
    #[arg(long, default_value_t = 1_500)]
    keytree_ban: usize,
    /// 데몬 노드를 띄워 키 그룹을 잴 크루 기기 수(쉼표로 여럿). 지어낸 멤버로 채우고 확정 기기·실제 멤버·실제 가입자는
    /// 로컬 TCP로 잇습니다.
    #[arg(long, value_delimiter = ',')]
    keytree_daemon: Vec<usize>,
    /// `--keytree-daemon`의 밴 물결 기기 수(50대씩 사용자 단위).
    #[arg(long, default_value_t = 1_500)]
    keytree_daemon_ban: usize,
    /// `--keytree-daemon`의 기록 나이(일): 하루에 가입 1명·추방 1명, 7일마다 전체 재키.
    #[arg(long, default_value_t = 365)]
    keytree_daemon_days: usize,
    /// `--keytree-daemon`에서 직접 인코딩 크루의 탈퇴를 잴 기기 수(0이면 건너뜀).
    #[arg(long, default_value_t = 200)]
    keytree_daemon_direct: usize,
    /// `--keytree-daemon`의 평상시 메시지 수.
    #[arg(long, default_value_t = 200)]
    keytree_daemon_messages: usize,
    /// 기록·그래프 시드.
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// 꺼져 있던 시간들(예: 1h,1d,1w).
    #[arg(long, value_delimiter = ',')]
    catchup: Vec<String>,
    /// 따라잡기 크루의 기기 수(공간 스트림의 멤버 변동 기록 크기).
    #[arg(long, default_value_t = 45_000)]
    catchup_devices: usize,
    /// 따라잡기 크루의 하루 메시지 수(채널 100개에 지프 분포로).
    #[arg(long, default_value_t = 100_000.0)]
    messages_per_day: f64,
    /// gossip 그래프: `노드 수:연결 수 목록`(예: 45000:16,32,64).
    #[arg(long)]
    gossip: Option<String>,
    /// gossip 구독자 밀도(0~1, 쉼표로 여럿).
    #[arg(long, value_delimiter = ',', default_value = "0.004,0.01,0.05,1")]
    density: Vec<f64>,
    /// 홉 하나의 지연 중앙값,p95(밀리초). 따라잡기 RTT는 중앙값의 두 배.
    #[arg(long, value_delimiter = ',', default_value = "250,1000")]
    hop_ms: Vec<f64>,
    /// 큰 크루 상태의 저장·색인 비용: `사용자:기기`(예: 30000:45000).
    #[arg(long)]
    store: Option<String>,
    /// 가입 키 묶음의 키 수(쉼표로 여럿): 묶음이 공간 스트림에 있을 때와 받은편지함에 있을 때 다른 멤버가 받는 양.
    #[arg(long, value_delimiter = ',')]
    join_inbox: Vec<usize>,
    /// `--join-inbox`의 크루 멤버 수.
    #[arg(long, default_value_t = 30_000)]
    join_members: usize,
    /// 체크포인트 앞 가입 멤버 수(쉼표로 여럿): 새 멤버가 받는 양(스냅숏 + 꼬리), 차가운 시작, 해소기 항목 수.
    #[arg(long, value_delimiter = ',')]
    checkpoint: Vec<usize>,
    /// `--checkpoint`의 체크포인트 뒤 가입 수(꼬리). 기본값은 다음 체크포인트 문턱(상태 이벤트 1,000개)의 절반.
    #[arg(long, default_value_t = 500)]
    checkpoint_tail: usize,
    /// 모든 결과를 JSON으로도 저장.
    #[arg(long)]
    json: Option<PathBuf>,
}

impl ScaleArgs {
    fn has_new(&self) -> bool {
        !self.fanout.is_empty()
            || !self.concurrent.is_empty()
            || self.partition
            || !self.keys.is_empty()
            || !self.keytree_real.is_empty()
            || !self.keytree_daemon.is_empty()
            || !self.catchup.is_empty()
            || self.gossip.is_some()
            || self.store.is_some()
            || !self.join_inbox.is_empty()
            || !self.checkpoint.is_empty()
    }
}

/// `--json`에 쓰는 모음.
#[derive(Serialize, Default)]
struct Output {
    state: Vec<StateReport>,
    scenarios: Vec<ScenarioReport>,
    cost_model: Option<CostModel>,
    keys: Vec<KeyRun>,
    keytree_real: Vec<RealRun>,
    keytree_daemon: Vec<DaemonRun>,
    catchup: Vec<CatchupRow>,
    gossip: Vec<GossipRow>,
    store: Option<scale::store::StoreReport>,
    join_inbox: Vec<JoinInboxRow>,
    checkpoint: Vec<CheckpointReport>,
}

fn kb(b: f64) -> f64 {
    b / 1024.0
}

/// 바이트를 읽기 좋게.
/// 바이트를 세 자리마다 쉼표로(목표 표는 단위 혼동이 없게 바이트 그대로 적음).
fn exact(b: f64) -> String {
    let s = format!("{:.0}", b.max(0.0));
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out + "B"
}

/// 표의 KB·MB는 이진 단위(1KB = 1,024B, 1MB = 1,048,576B)입니다.
fn bytes(b: f64) -> String {
    if b >= 1024.0 * 1024.0 {
        format!("{:.2}MB", b / 1024.0 / 1024.0)
    } else if b >= 1024.0 {
        format!("{:.1}KB", b / 1024.0)
    } else {
        format!("{b:.0}B")
    }
}

pub fn run(args: ScaleArgs) -> Result<()> {
    let mut out = Output::default();
    // 옵션 형식은 오래 도는 표보다 먼저 확인합니다.
    let hop = hop(&args.hop_ms)?;
    for s in &args.catchup {
        catchup::parse_duration(s).with_context(|| format!("모르는 시간: {s}(예: 1h, 1d, 1w)"))?;
    }
    let legacy = !args.has_new();
    let members = args
        .members
        .clone()
        .or_else(|| legacy.then(|| vec![1_000, 5_000, 10_000, 12_000]))
        .unwrap_or_default();
    let msg = scale::message()?;
    println!(
        "짧은 글 이벤트 {:.2}KB(그중 서명 {:.2}KB)",
        kb(msg.short_text as f64),
        kb(msg.sig as f64)
    );
    if !members.is_empty() {
        out.state = state_table(&members)?;
    }
    if !args.fanout.is_empty() || !args.concurrent.is_empty() || args.partition {
        out.scenarios = scenarios(&args)?;
    }
    if !args.keys.is_empty() {
        let model = CostModel::default();
        out.keys = key_tables(&args, &model)?;
        out.cost_model = Some(model);
    }
    if !args.keytree_real.is_empty() {
        let model = out.cost_model.clone().unwrap_or_default();
        out.keytree_real = keytree_real_table(&args, &model)?;
    }
    if !args.keytree_daemon.is_empty() {
        out.keytree_daemon = keytree_daemon_table(&args)?;
    }
    if !args.catchup.is_empty() {
        let join = out
            .state
            .first()
            .map(|s| s.join_wire)
            .map_or_else(|| scale::state(2).map(|s| s.join_wire), Ok)?;
        out.catchup = catchup_table(&args, msg.short_text, join, hop)?;
    }
    if let Some(g) = &args.gossip {
        out.gossip = gossip_table(g, &args.density, hop)?;
    }
    if let Some(spec) = &args.store {
        out.store = Some(store_table(spec)?);
    }
    if !args.join_inbox.is_empty() {
        let join = out
            .state
            .first()
            .map(|s| s.join_wire)
            .map_or_else(|| scale::state(2).map(|s| s.join_wire), Ok)?;
        out.join_inbox = join_inbox_table(&args, join)?;
    }
    if !args.checkpoint.is_empty() {
        out.checkpoint = checkpoint_table(&args)?;
    }
    if let Some(path) = &args.json {
        let mut b = serde_json::to_vec_pretty(&out)?;
        b.push(b'\n');
        std::fs::write(path, b).with_context(|| format!("JSON 저장 실패: {}", path.display()))?;
        println!();
        println!("JSON: {}", path.display());
    }
    Ok(())
}

fn store_table(spec: &str) -> Result<scale::store::StoreReport> {
    let (u, d) = spec
        .split_once(':')
        .context("--store는 '사용자:기기'(예: 30000:45000)")?;
    let users: usize = u.trim().parse().context("--store 사용자 수")?;
    let devices: usize = d.trim().parse().context("--store 기기 수")?;
    let exe = std::env::current_exe()?;
    let r = scale::store::measure(users, devices, &exe)?;
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
    println!();
    println!(
        "#### 상태 저장·색인 (사용자 {}명, 기기 {}대, 채널 {}개)",
        r.users, r.devices, 100
    );
    println!();
    println!(
        "짓기 {:.0}ms, 처음 통째로 쓰기 {:.0}ms(상태 행 값 {:.1}MB), DB {:.1}MB(대부분 기기 키 표)",
        ms(r.build),
        ms(r.full_write),
        kb(kb(r.state_bytes as f64)),
        r.db_mb
    );
    println!();
    println!(
        "| 바꾸기 | 횟수 | 이벤트 본문 | CPU 중앙값 | CPU 최대 | 벽시계 최대 | 행 | 값 바이트 | 이벤트·키 WAL 중앙값 | 이벤트·키 WAL 최대 | 상태 WAL 중앙값 | 상태 WAL 최대 | 합계 WAL 최대 |"
    );
    println!(
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    );
    for c in &r.changes {
        println!(
            "| {} | {} | {} | {:.2}ms | {:.2}ms | {:.2}ms | {} | {} | {:.1}KB | {:.1}KB | {:.1}KB | {:.1}KB | {:.1}KB |",
            c.kind,
            c.samples,
            exact(c.event_body as f64),
            c.cpu_median_us / 1e3,
            c.cpu_max_us / 1e3,
            c.wall_max_us / 1e3,
            c.rows,
            exact(c.value_bytes as f64),
            kb(c.wal_event_median as f64),
            kb(c.wal_event_max as f64),
            kb(c.wal_state_median as f64),
            kb(c.wal_state_max as f64),
            kb(c.wal_total_max as f64),
        );
    }
    println!();
    let parts = |p: &[std::time::Duration; 3]| {
        format!(
            "{:.1}ms(기기 목록 {:.1}, 채널 둘 {:.1}, 기기 → 사용자 {:.1})",
            ms(p.iter().sum()),
            ms(p[0]),
            ms(p[1]),
            ms(p[2])
        )
    };
    println!(
        "상태 색인(전파 한 번 몫): 새 사본 처음 {}, 같은 사본 다시 {:.1}µs, 멤버 하나 바뀐 다음 사본 처음 {}",
        parts(&r.index.first),
        r.index.again.as_secs_f64() * 1e6,
        parts(&r.index.next)
    );
    let c = &r.cold;
    let mb = |v: Option<f64>| v.map_or_else(|| "-".to_string(), |m| format!("{m:.0}MB"));
    println!(
        "처음 읽기(새 프로세스): 열기 {:.0}ms, 상태 행 읽기 {:.0}ms(멤버 {}명, 기기 {}대), 색인 {:.1}ms, 키 {}개 {:.0}ms; \
         RSS {}(읽기 전 {}), 최대 RSS {}",
        ms(c.open),
        ms(c.load),
        c.members,
        c.devices,
        ms(c.index),
        4096,
        ms(c.keys),
        mb(c.rss_mb),
        mb(c.base_rss_mb),
        mb(c.peak_rss_mb),
    );
    let p = &r.replay;
    println!(
        "다시 세우기(데몬 시작) 서명 확인: 가입 이벤트 하나 {:.0}µs + 기기 목록 하나 {:.0}µs, 사용자 {}명이면 {:.1}초 \
         (초대 증명·프로필 확인, 복호화, 해소는 빠짐)",
        p.event_verify.as_secs_f64() * 1e6,
        p.list_verify.as_secs_f64() * 1e6,
        r.users,
        p.joins_estimate.as_secs_f64(),
    );
    Ok(r)
}

fn hop(v: &[f64]) -> Result<HopLatency> {
    match v {
        [m, p] if *m > 0.0 && p >= m => Ok(HopLatency {
            median_ms: *m,
            p95_ms: *p,
        }),
        _ => bail!("--hop-ms는 '중앙값,p95'(p95 ≥ 중앙값 > 0)"),
    }
}

fn state_table(members: &[usize]) -> Result<Vec<StateReport>> {
    println!();
    println!(
        "| 멤버 | 적용 | 거부(첫 사유) | 가입 기록 | 공간 스트림 | 본문(RAM) | 만들기 | 하나씩 해소 | 다시 해소 | 동시 가입 해소 | 최대 RSS | 옛 부모 3% 하나씩 |"
    );
    println!(
        "| ---: | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    );
    let mut out = Vec::new();
    for &n in members {
        let r = scale::state(n)?;
        let s = scale::stale_chain(n, 3)?;
        let stale = format!(
            "{:.1?}(동시 {}건, 비증분 {}회 평균 {:.1?}, 전체 {}·재생 {}회, 재생 평균 {}항목)",
            s.insert,
            s.stale,
            s.concurrent_inserts,
            s.concurrent / s.concurrent_inserts.max(1) as u32,
            s.stats.full,
            s.stats.replay,
            s.stats.replayed / s.stats.replay.max(1),
        );
        println!(
            "| {} | {} | {}{} | {:.1}KB | {:.1}MB | {:.1}MB | {:.1?} | {:.1?} | {:.1?} | {:.1?} | {} | {} |",
            r.members,
            r.applied,
            r.rejected,
            r.first_reject
                .as_ref()
                .map(|e| format!(" ({e})"))
                .unwrap_or_default(),
            kb(r.join_wire as f64),
            kb(kb(r.stream_wire as f64)),
            kb(kb(r.bodies as f64)),
            r.generate,
            r.insert,
            r.rebuild,
            r.concurrent,
            r.peak_rss_mb
                .map(|m| format!("{m:.0}MB"))
                .unwrap_or_else(|| "-".into()),
            stale,
        );
        out.push(r);
    }
    Ok(out)
}

fn scenarios(args: &ScaleArgs) -> Result<Vec<ScenarioReport>> {
    let mut out = Vec::new();
    for &k in &args.fanout {
        out.push(scale::fanout_joins(k)?);
    }
    for &pct in &args.concurrent {
        out.push(scale::concurrent_branches(args.crew, pct, args.seed)?);
    }
    if args.partition {
        // 한 시간 갈라짐 동안 한쪽이 받아 주는 가입: 하루 1%의 1/24, 적어도 10명.
        let side = (args.crew / 100 / 24).max(10);
        out.push(scale::partition_merge(args.crew, side)?);
    }
    println!();
    println!("### 해소기 시나리오");
    println!();
    println!(
        "| 시나리오 | 변수 | 멤버 | 넣은 이벤트 | 거부 | 이어서 적용 | 처음부터 / 스냅숏 재생 | 넣기 전체 | 가장 긴 한 번 | 끝난 뒤 다시 해소 | 한 묶음 | 같은 상태 | 해소기 RSS 증가 | 최대 RSS | 새 멤버 받기 | 만들기 |"
    );
    println!(
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- | ---: | ---: | ---: | ---: |"
    );
    for r in &out {
        let param = match r.scenario {
            "동시 가입" => format!("K={}", r.param),
            "늦게 온 갈래" => format!("{}%", r.param),
            _ => format!("한쪽 {}명", r.param),
        };
        println!(
            "| {} | {} | {} | {} | {}{} | {} | {} / {} | {:.1?} | {:.1?} | {:.1?} | {} | {} | {} | {} | {} | {:.1?} |",
            r.scenario,
            param,
            r.members,
            r.events,
            r.rejected,
            r.first_reject
                .as_ref()
                .map(|e| format!(" ({e})"))
                .unwrap_or_default(),
            r.incremental,
            r.rebuilds,
            r.replays,
            r.insert,
            r.slowest,
            r.rebuild,
            r.batch.map_or_else(|| "-".into(), |d| format!("{d:.1?}")),
            r.converged.map_or("-", |c| if c { "예" } else { "아니오" }),
            r.resolver_rss_mb
                .map_or_else(|| "-".into(), |m| format!("{m:.0}MB")),
            r.peak_rss_mb
                .map_or_else(|| "-".into(), |m| format!("{m:.0}MB")),
            bytes(r.newcomer_bytes as f64),
            r.generate,
        );
    }
    Ok(out)
}

fn scheme_of(s: &str) -> Result<SchemeKind> {
    Ok(match s.trim() {
        "legacy" | "mls" => SchemeKind::LegacyMls,
        "brief" | "subgroups" => SchemeKind::BriefSubgroups,
        "tree" | "keytree" => SchemeKind::KeyTree,
        other => bail!("모르는 방식: {other}(legacy, brief, tree)"),
    })
}

fn key_tables(args: &ScaleArgs, model: &CostModel) -> Result<Vec<KeyRun>> {
    let kinds: Vec<SchemeKind> = args
        .schemes
        .iter()
        .map(|s| scheme_of(s))
        .collect::<Result<_>>()?;
    let mut out = Vec::new();
    for &devices in &args.keys {
        let t = Instant::now();
        let trace = trace::generate(&Churn::new(devices, args.days, args.seed));
        let counts = trace.counts();
        println!();
        println!(
            "## 키 관리 모형: 처음 기기 {}대, 사용자 {}명, {}일 (기록 {:.1?})",
            trace.initial_devices(),
            trace.initial.len(),
            args.days,
            t.elapsed()
        );
        println!();
        println!(
            "변동: {}",
            counts
                .iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        for &kind in &kinds {
            let track = kind != SchemeKind::LegacyMls || devices <= LEGACY_TRACK_MAX;
            let opts = RunOpts {
                track,
                ..RunOpts::default()
            };
            let r = keys::run(kind, &trace, model, opts);
            print_key_run(&r, kind);
            out.push(r);
        }
    }
    Ok(out)
}

/// 초를 읽기 좋게(1초 아래는 ms).
fn dur(s: f64) -> String {
    if s < 1.0 {
        format!("{:.2}ms", s * 1e3)
    } else {
        format!("{s:.2}초")
    }
}

fn keytree_real_table(args: &ScaleArgs, model: &CostModel) -> Result<Vec<RealRun>> {
    let mut out = Vec::new();
    for &n in &args.keytree_real {
        let r = keyreal::run(n, args.keytree_ban, args.seed, model)
            .with_context(|| format!("실제 키 트리 {n}대"))?;
        println!();
        println!(
            "## 실제 키 트리 대 모형: 크루 기기 {}대, 사용자 {}명, 밴 기기 {}대, 깊이 {} (처음 구성 {}, 모형 {})",
            r.devices,
            r.users,
            r.banned_devices,
            r.depth,
            dur(r.setup_secs),
            dur(r.model_secs)
        );
        println!();
        println!(
            "| 작업 | 실제 | 모형 | 차이 | 목표 | 판정 | 확정 기기 CPU 실제 / 모형 | 멤버 적용 | 비고 |"
        );
        println!("| --- | ---: | ---: | ---: | ---: | --- | ---: | ---: | --- |");
        let secs = |v: Option<f64>| v.map_or_else(|| "-".into(), dur);
        for row in &r.rows {
            let target = match (row.target, row.cpu_target) {
                (Some(b), Some(c)) => format!("≤ {} · ≤ {c:.0}초", exact(b)),
                (Some(b), None) => format!("≤ {}", exact(b)),
                (None, Some(c)) => format!("CPU ≤ {c:.0}초"),
                (None, None) => "-".into(),
            };
            println!(
                "| {} | {} | {} | {} | {} | {} | {} / {} | {} | {} |",
                row.op,
                exact(row.real),
                row.model.map_or_else(|| "-".into(), exact),
                row.diff()
                    .map_or_else(|| "-".into(), |d| format!("{d:+.1}%")),
                target,
                row.meets()
                    .map_or("-", |ok| if ok { "충족" } else { "못 미침" }),
                secs(row.real_cpu),
                secs(row.model_cpu),
                secs(row.member_secs),
                row.detail,
            );
        }
        out.push(r);
    }
    Ok(out)
}

fn keytree_daemon_table(args: &ScaleArgs) -> Result<Vec<DaemonRun>> {
    let mut out = Vec::new();
    for &n in &args.keytree_daemon {
        let opts = DaemonOpts {
            devices: n,
            ban: args.keytree_daemon_ban.min(n / 2),
            direct: args.keytree_daemon_direct,
            days: args.keytree_daemon_days,
            messages: args.keytree_daemon_messages,
            step_secs: 600,
        };
        let started = Instant::now();
        let r = keydaemon::run(&opts).with_context(|| format!("데몬 키 그룹 {n}대"))?;
        println!();
        println!(
            "## 데몬 키 그룹: 크루 기기 {}대, 사용자 {}명, 깊이 {} (채우기 {}, 전체 {})",
            r.devices,
            r.users,
            r.depth,
            dur(r.setup_secs),
            dur(started.elapsed().as_secs_f64())
        );
        println!();
        println!("| 작업 | 바이트 | 목표 | 판정 | 확정 기기 벽시계 | 비고 |");
        println!("| --- | ---: | ---: | --- | ---: | --- |");
        for row in &r.rows {
            println!(
                "| {} | {} | {} | {} | {} | {} |",
                row.op,
                exact(row.bytes),
                row.target
                    .map_or_else(|| "-".into(), |t| format!("≤ {}", exact(t))),
                row.meets()
                    .map_or("-", |ok| if ok { "충족" } else { "못 미침" }),
                row.secs.map_or_else(|| "-".into(), dur),
                row.detail,
            );
        }
        println!();
        println!(
            "평상시 키 묶음 {}개, 불변식 검사 {}번 중 어긋남 {}번{}",
            r.steady_key_shares,
            r.checks,
            r.violations.len(),
            if r.violations.is_empty() {
                String::new()
            } else {
                format!(": {:?}", r.violations)
            }
        );
        out.push(r);
    }
    Ok(out)
}

fn print_key_run(r: &KeyRun, kind: SchemeKind) {
    println!();
    println!(
        "### {} — 기기 {}, 사용자 {}, {}일, 변동 {}건 ({:.1}초)",
        r.scheme,
        r.devices,
        r.users,
        r.days,
        r.ops,
        r.elapsed_ms as f64 / 1000.0
    );
    println!();
    println!(
        "| 갈래 | 그룹 | 횟수 | 이벤트 | 발행 | 그룹 기기당 받기 평균 / p95 / 최대 | 다른 크루 기기 | 대상 기기 | X-Wing | 서명 | 최대 본문 | 한도 넘음 |"
    );
    println!("| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
    for c in &r.classes {
        println!(
            "| {} | {} | {} | {:.1} | {} | {} / {} / {} | {} | {} | {:.1} | {:.1} | {} | {} |",
            c.class.label(),
            if c.crew { "크루" } else { "비공개" },
            c.count,
            c.mean(|o| o.events),
            bytes(c.mean(|o| o.published)),
            bytes(c.mean(|o| o.own)),
            bytes(c.own_p95 as f64),
            bytes(c.max.own as f64),
            bytes(c.mean(|o| o.others)),
            bytes(c.mean(|o| o.target)),
            c.mean(|o| o.xwing),
            c.mean(|o| o.signatures),
            bytes(c.max.max_body as f64),
            c.sum.over_cap,
        );
    }
    let i = &r.invariants;
    println!();
    if i.tracked {
        println!(
            "불변식: I1 {}(기기 {}), I2 {}(기기 {}, 그중 낡은 키 {}), I3 {}(기기 {}), I4 {}(최대 본문 {}) — 검사 기기 {}/{}, 키 {}, 간선 {}, 봉인 {}",
            i.i1,
            i.i1_devices,
            i.i2,
            i.i2_devices,
            i.i2_stale,
            i.i3,
            i.i3_devices,
            i.i4,
            bytes(i.i4_max_body as f64),
            i.checked,
            i.devices,
            i.keys,
            i.edges,
            i.seals
        );
    } else {
        println!(
            "불변식: 기호 추적 끔(기기 {LEGACY_TRACK_MAX}대 넘음), I4 {}(최대 본문 {})",
            i.i4,
            bytes(i.i4_max_body as f64)
        );
    }
    for e in &i.examples {
        println!("- {e}");
    }
    println!(
        "확정 기기 CPU(기록 전체, 처음 구성 제외): X-Wing {}회, 서명 {}회 → 명령 {:.2}G, 약 {:.1}초 (가장 무거운 작업 {:.2}초)",
        r.cpu.xwing,
        r.cpu.signatures,
        r.cpu.instructions as f64 / 1e9,
        r.cpu.seconds,
        r.cpu_max_op.seconds
    );
    println!(
        "비공개 그룹 밖 크루 기기 하나가 하루에 받는 키 바이트: {}",
        bytes(r.plain_device_per_day)
    );
    for (k, v) in &r.notes {
        println!("- {k}: {v}");
    }
    if kind == SchemeKind::KeyTree {
        targets(r);
    }
}

/// 수정판 목표와 비교(맞추려고 바꾸지 않고 그대로 보여 줌). 목표는 십진 단위(1kB = 1,000B, 1MB = 1,000,000B)로
/// 판정합니다. 이진 단위로 읽으면 2~5% 느슨해져 판정이 바뀔 수 있어, 결과를 바이트 그대로 적습니다.
fn targets(r: &KeyRun) {
    let crew = |c: CostClass| r.class(c, true);
    let row = |name: &str, goal: f64, got: Option<f64>| {
        let (got_s, ok) = match got {
            Some(v) => (
                format!("{} ({})", exact(v), bytes(v)),
                if v <= goal { "맞음" } else { "넘음" },
            ),
            None => ("-".into(), "기록에 없음"),
        };
        println!("| {name} | ≤ {} | {got_s} | {ok} |", exact(goal));
    };
    println!();
    println!("| 수정판 목표(십진 단위) | 목표 | 결과(괄호는 이진 단위) | 판정 |");
    println!("| --- | ---: | ---: | --- |");
    row(
        "탈퇴·추방: 크루 기기당 받기(p95)",
        4_500.0,
        crew(CostClass::Removal).map(|c| c.own_p95 as f64),
    );
    row(
        "가입: 크루 기기당 받기(평균)",
        2_000.0,
        crew(CostClass::Join).map(|c| c.mean(|o| o.own)),
    );
    row(
        "가입: 가입자 받기(평균)",
        14_000.0,
        crew(CostClass::Join).map(|c| c.mean(|o| o.target)),
    );
    row(
        "전체 재키: 기기당 받기(자기 사용자 조각 + 증명 + 머리 이벤트, 평균)",
        5_000.0,
        crew(CostClass::FullRekey).map(|c| c.mean(|o| o.own)),
    );
    row(
        "전체 재키: 블롭에서 사용자 하나의 조각",
        5_000.0,
        r.notes
            .iter()
            .find(|(k, _)| k == "전체 재키 블롭/사용자")
            .and_then(|(_, v)| v.trim_end_matches('B').parse().ok()),
    );
    // 밴 물결은 크루 그룹 `KeyUpdate`만 셀 때(설계 표의 범위)와 비공개 그룹 회전까지 더할 때를 함께 봅니다.
    let ban = |crew: bool| {
        r.class(CostClass::Ban, crew)
            .map(|c| c.sum.published as f64)
    };
    row("밴 물결: 크루 그룹 발행", 1_600_000.0, ban(true));
    row(
        "밴 물결: 크루 + 비공개 그룹 발행",
        1_600_000.0,
        ban(true).map(|c| c + ban(false).unwrap_or(0.0)),
    );
    let v = r.invariants.violations();
    println!(
        "| 불변식 위반 | 0 | {v} | {} |",
        if v == 0 { "맞음" } else { "넘음" }
    );
}

fn catchup_table(
    args: &ScaleArgs,
    message_bytes: usize,
    join_bytes: usize,
    hop: HopLatency,
) -> Result<Vec<CatchupRow>> {
    let offline = args
        .catchup
        .iter()
        .map(|s| {
            catchup::parse_duration(s).with_context(|| format!("모르는 시간: {s}(예: 1h, 1d, 1w)"))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut spec = CatchupSpec::new(args.catchup_devices, offline);
    spec.messages_per_day = args.messages_per_day;
    spec.message_bytes = message_bytes as u32;
    spec.join_bytes = join_bytes as u32;
    spec.rtt_ms = 2.0 * hop.median_ms;
    spec.seed = args.seed;
    let t = Instant::now();
    let rows = catchup::catchup(&spec);
    println!();
    println!(
        "### 따라잡기: 크루 기기 {}대, 하루 메시지 {}개(채널 {}개, 지프 {}), 메시지 {}, 가입 기록 {}, RTT {:.0}ms ({:.1?})",
        spec.devices,
        spec.messages_per_day,
        spec.channels,
        spec.zipf,
        bytes(spec.message_bytes as f64),
        bytes(spec.join_bytes as f64),
        spec.rtt_ms,
        t.elapsed()
    );
    println!();
    println!(
        "| 꺼진 시간 | 스트림 | 새 이벤트 | 요청(머리/가져오기/지문/ID) | 부모 따라가기 | 지문 비교 | 받은 ID | 오버헤드 올림/받음 | 이벤트 | 왕복×RTT | 완료 |"
    );
    println!("| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |");
    for r in &rows {
        let off = human_ms(r.offline_ms);
        let line = |name: String, s: &StreamCatchup| {
            println!(
                "| {off} | {name} | {} | {} ({}/{}/{}/{}) | {} | {} | {} | {} / {} | {} | {:.0}초 | {} |",
                s.new_events,
                s.requests(),
                s.heads,
                s.get_events,
                s.fingerprints,
                s.get_ids,
                s.chase_rounds,
                s.reconciled,
                s.ids_listed,
                bytes(s.overhead_up as f64),
                bytes(s.overhead_down as f64),
                bytes(s.event_bytes as f64),
                s.requests() as f64 * spec.rtt_ms / 1000.0,
                if s.complete { "예" } else { "아니오" },
            );
        };
        line("공간".into(), &r.space);
        line("가장 바쁜 채널".into(), &r.busiest);
        line(
            format!("채널 {}개(새 것 없음 {})", spec.channels, r.idle_channels),
            &r.channels,
        );
        line("합계".into(), &r.total);
    }
    Ok(rows)
}

fn join_inbox_table(args: &ScaleArgs, join_bytes: usize) -> Result<Vec<JoinInboxRow>> {
    let t = Instant::now();
    let rows = args
        .join_inbox
        .iter()
        .map(|&k| inbox::join_inbox(args.join_members, k, join_bytes))
        .collect::<Result<Vec<_>>>()?;
    println!();
    println!(
        "### 가입 키 묶음: 멤버 {}명, 가입자 기기 {}대, 키 {}개씩, 하루 가입 멤버의 {:.0}% ({:.1?})",
        args.join_members,
        inbox::JOINER_DEVICES,
        inbox::KEYS_PER_SHARE,
        inbox::JOINS_PER_DAY * 100.0,
        t.elapsed()
    );
    println!();
    println!(
        "| 묶음 키 | 묶음 이벤트 | 가입 기록 | 묶음(예전/지금) | 다른 멤버 1명·가입 1회 예전 → 지금 | 다른 멤버 1명·하루 예전 → 지금 | 크루 전체·가입 1회 예전 → 지금 |"
    );
    println!("| ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
    for r in &rows {
        println!(
            "| {} | {} | {} | {} / {} | {} → {} | {} → {} | {} → {} |",
            r.keys,
            r.bundle_events,
            bytes(r.join_wire as f64),
            bytes(r.bundle_before as f64),
            bytes(r.bundle_after as f64),
            bytes(r.member_per_join_before as f64),
            bytes(r.member_per_join_after as f64),
            bytes(r.member_per_day_before as f64),
            bytes(r.member_per_day_after as f64),
            bytes(r.crew_per_join_before as f64),
            bytes(r.crew_per_join_after as f64),
        );
    }
    Ok(rows)
}

fn checkpoint_table(args: &ScaleArgs) -> Result<Vec<CheckpointReport>> {
    let exe = std::env::current_exe()?;
    let rows = args
        .checkpoint
        .iter()
        .map(|&n| scale::checkpoint::measure(n, args.checkpoint_tail, Some(&exe)))
        .collect::<Result<Vec<_>>>()?;
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
    let mb = |m: Option<f64>| m.map_or("-".into(), |m| format!("{m:.0}MB"));
    println!();
    println!(
        "### 상태 체크포인트: 체크포인트 뒤 가입 {}개(꼬리)",
        args.checkpoint_tail
    );
    println!();
    println!(
        "| 멤버 | 전체 기록 | 새 멤버가 받는 양 | 스냅숏 평문 / 블롭 | 체크포인트 이벤트 | 경계 위 이벤트 | 초안 / 암호화 / 다음 체크포인트 넣기 |"
    );
    println!("| ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
    for r in &rows {
        println!(
            "| {} | {} | {} | {} / {} | {} | {}개(조상 {}개), {} | {:.0}ms / {:.0}ms / {:.0}ms |",
            r.members,
            bytes(r.full_wire as f64),
            bytes(r.newcomer_wire as f64),
            bytes(r.snapshot_plain as f64),
            bytes(r.snapshot_blob as f64),
            bytes(r.checkpoint_wire as f64),
            r.above_floor,
            r.frontier,
            bytes(r.above_floor_wire as f64),
            ms(r.draft),
            ms(r.seal),
            ms(r.compact),
        );
    }
    println!();
    println!(
        "| 멤버 | 차가운 시작 | 읽기 | 서명 확인·복호화 | 해소 | 상주 메모리 증가 / 최대 | 해소기 항목 |"
    );
    println!("| ---: | --- | ---: | ---: | ---: | ---: | ---: |");
    for r in &rows {
        for (name, c, entries) in [
            ("처음부터", &r.cold_full, r.entries_full),
            ("스냅숏 + 경계 위", &r.cold_checkpoint, r.entries_newcomer),
        ] {
            println!(
                "| {} | {} | {:.0}ms | {:.0}ms | {:.0}ms | {} / {} | {} (그 노드에서 {}) |",
                r.members,
                name,
                ms(c.read),
                ms(c.open),
                ms(c.resolve),
                mb(c.rss_mb),
                mb(c.peak_rss_mb),
                c.entries,
                entries,
            );
        }
    }
    println!();
    println!(
        "전체 기록 노드의 해소기 항목(체크포인트 앞 → 다음 체크포인트를 받은 뒤): {}",
        rows.iter()
            .map(|r| format!("{} → {}", r.entries_full, r.entries_compacted))
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(rows)
}

fn human_ms(ms: u64) -> String {
    const H: u64 = 3_600_000;
    if ms.is_multiple_of(7 * 24 * H) {
        format!("{}주", ms / (7 * 24 * H))
    } else if ms.is_multiple_of(24 * H) {
        format!("{}일", ms / (24 * H))
    } else if ms.is_multiple_of(H) {
        format!("{}시간", ms / H)
    } else {
        format!("{}분", ms / 60_000)
    }
}

fn gossip_table(spec: &str, densities: &[f64], hop: HopLatency) -> Result<Vec<GossipRow>> {
    let (nodes, ks) = spec
        .split_once(':')
        .context("--gossip은 '노드 수:연결 수 목록'(예: 45000:16,32,64)")?;
    let nodes: usize = nodes.trim().parse().context("노드 수")?;
    let ks: Vec<usize> = ks
        .split(',')
        .map(|k| k.trim().parse().context("연결 수"))
        .collect::<Result<_>>()?;
    if ks.iter().any(|k| *k == 0 || *k >= nodes) {
        bail!("연결 수는 1 이상, 노드 수 미만");
    }
    let t = Instant::now();
    let rows = gossip::table(nodes, &ks, densities, hop);
    println!();
    println!(
        "### gossip 메시: 노드 {nodes}, 홉 지연 중앙값 {}ms·p95 {}ms, mesh_n {}/{}/{}, 그래프 2개 × 작성자 24명 ({:.1?})",
        hop.median_ms,
        hop.p95_ms,
        gossip::MESH_N_LOW,
        gossip::MESH_N,
        gossip::MESH_N_HIGH,
        t.elapsed()
    );
    println!();
    println!(
        "| 연결 | 구독 비율 | 구독자 | 연결 방식 | 구독 이웃 | P(메시) | 메시 덩어리 | 메시·flood 도달 | 전체 도달 | p50 | p95 | 홉(1/2/3/4/5+) | 소문만 | 직접 받음 | NoPeers 작성자 | 올림 flood 켬/끔 |"
    );
    println!(
        "| ---: | ---: | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- | ---: | ---: | ---: | ---: |"
    );
    let pct = |v: f64| format!("{:.1}%", v * 100.0);
    let ms = |v: Option<f64>| v.map_or_else(|| "못 닿음".into(), |v| format!("{:.0}ms", v));
    for r in &rows {
        let mut h = [0.0f64; 5];
        for (i, v) in r.hops.iter().enumerate() {
            h[i.min(4)] += v;
        }
        println!(
            "| {} | {} | {} | {} | {:.1} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {:.1} / {:.1} |",
            r.k,
            pct(r.density),
            r.subscribers,
            if r.topic_aware {
                "토픽 인지"
            } else {
                "무작위"
            },
            r.sub_peers,
            pct(r.p_mesh),
            pct(r.mesh_component),
            pct(r.mesh_reach),
            pct(r.delivered),
            ms(r.p50_ms),
            ms(r.p95_ms),
            h.iter()
                .map(|v| format!("{:.0}", v * 100.0))
                .collect::<Vec<_>>()
                .join("/"),
            pct(r.lazy_only),
            pct(r.direct_only),
            pct(r.no_peers),
            r.upload_flood,
            r.upload_no_flood,
        );
    }
    Ok(rows)
}
