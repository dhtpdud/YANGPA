//! `yangpa-bench`: 성능 기준 실행·측정·회귀 검사.
//!
//! - `run <작업> <n>`: 준비 뒤 `n`번 반복(valgrind 아래에서 부르는 단위). 반복 부분만 cachegrind가 셉니다.
//! - `measure`: 작업마다 `valgrind --tool=cachegrind`로 반복 수만 다른 두 번을 재 한 번당 명령 수를 냅니다. 준비는
//!   세지 않습니다: FN-DSA 서명은 무작위 거절 표본 추출이라 이벤트를 서명해 두는 준비의 명령 수가 실행마다 수백만씩
//!   달라, 두 번의 차이에 섞이면 한 번당 값이 허용치보다 크게 흔들림.
//! - `gate`: `measure` 결과를 기준선(`baseline.json`)과 비교해 허용치를 넘게 늘었으면 실패합니다. 명령 수는
//!   컴파일러 판에 따라 달라지므로, 기준선을 잰 툴체인과 지금 툴체인이 다르면(`--toolchain`) 비교는 참고용으로만
//!   찍고 통과합니다(`--strict`면 그래도 실패, CI는 툴체인을 기준선에 맞춰 고정하고 `--strict`로 돌림).
//! - `time`: 벽시계 시간(사람이 보는 용도).
//! - `scale`: 규모 시뮬레이션(큰 크루의 상태 크기와 시간, 해소기 시나리오, 키 관리 모형·실제 키 그룹, 따라잡기, gossip 메시,
//!   [`yangpa_bench::scale`]). 옵션과 표는 `scale_report`.

mod scale_report;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use yangpa_bench::{Spec, find, specs};

#[derive(Parser)]
#[command(name = "yangpa-bench", about = "양파 메신저 성능 기준")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 작업 목록.
    List,
    /// 준비 뒤 n번 반복합니다.
    Run { name: String, n: u64 },
    /// 한 번당 명령 수(valgrind cachegrind)를 잽니다.
    Measure {
        #[command(flatten)]
        m: MeasureArgs,
        /// 결과를 JSON으로 저장(기준선 갱신).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// 기준선과 비교해 느려졌으면 실패합니다.
    Gate {
        #[command(flatten)]
        m: MeasureArgs,
        #[arg(long, default_value = "crates/yangpa-bench/baseline.json")]
        baseline: PathBuf,
        /// 모든 작업에 쓸 허용 증가율(빼면 작업별 값).
        #[arg(long)]
        threshold: Option<f64>,
        /// 툴체인이 기준선과 달라도 허용치를 넘으면 실패.
        #[arg(long)]
        strict: bool,
        /// 이번 측정을 JSON으로도 저장.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// 규모 시뮬레이션(네트워크 없이): 큰 크루의 상태 크기·해소 시간, 해소기 시나리오, 키 관리 모형·실제 키 그룹,
    /// 따라잡기, gossip 메시.
    // 인자가 많아 상자에 넣음(다른 명령 크기를 키우지 않게).
    Scale(Box<scale_report::ScaleArgs>),
    /// `scale --store`가 부르는 새 프로세스: 저장된 크루를 처음 읽고 시간·상주 메모리를 JSON으로 찍습니다.
    #[command(hide = true)]
    ScaleLoad { dir: PathBuf },
    /// `scale --checkpoint`가 부르는 새 프로세스: 남긴 크루 기록으로 해소기를 세우고 시간·상주 메모리를 JSON으로 찍습니다.
    #[command(hide = true)]
    ScaleCheckpointLoad {
        dir: PathBuf,
        #[arg(value_enum)]
        mode: yangpa_bench::scale::checkpoint::ColdMode,
    },
    /// 벽시계 시간(한 번당).
    Time {
        /// 작업 이름(빼면 전부).
        names: Vec<String>,
        /// 반복 수(빼면 작업별 기본값).
        #[arg(long)]
        iters: Option<u64>,
    },
}

#[derive(clap::Args)]
struct MeasureArgs {
    /// 작업 이름(빼면 전부).
    names: Vec<String>,
    /// 적은 쪽 반복 수.
    #[arg(long, default_value_t = 2)]
    n1: u64,
    /// 많은 쪽 반복 수.
    #[arg(long, default_value_t = 10)]
    n2: u64,
    /// valgrind 실행 파일.
    #[arg(long, default_value = "valgrind")]
    valgrind: String,
    /// 빌드한 툴체인(예: `rustc --version` 출력). 기준선에 적고 비교 때 맞춰 봅니다.
    #[arg(long, env = "YANGPA_BENCH_TOOLCHAIN")]
    toolchain: Option<String>,
}

/// 기준선 파일.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Baseline {
    /// 어떻게 쟀는지(사람이 읽는 메모).
    #[serde(default)]
    note: String,
    /// 잰 바이너리를 빌드한 툴체인.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    toolchain: Option<String>,
    /// 작업 → 한 번당 명령 수.
    instructions: BTreeMap<String, u64>,
}

fn main() -> Result<()> {
    // 데몬 경로를 도는 재기에서 어디서 멈췄는지 볼 때만 켭니다(예: `YANGPA_BENCH_LOG=yangpa_daemon=debug`, 기본은 끔).
    if let Ok(filter) = tracing_subscriber::EnvFilter::try_from_env("YANGPA_BENCH_LOG") {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .try_init();
    }
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::List => {
            for s in specs() {
                println!("{:<24} {}", s.name, s.about);
            }
            Ok(())
        }
        Cmd::Run { name, n } => {
            let spec = find(&name).with_context(|| format!("없는 작업: {name}"))?;
            let mut w = (spec.setup)()?;
            cachegrind_request(CG_START_INSTRUMENTATION);
            let r = w.run(n);
            cachegrind_request(CG_STOP_INSTRUMENTATION);
            r
        }
        Cmd::Measure { m, out } => {
            let got = measure_all(&m)?;
            print_table(&got, None, None);
            if let Some(path) = out {
                save(&path, &got, m.toolchain.clone())?;
            }
            Ok(())
        }
        Cmd::Gate {
            m,
            baseline,
            threshold,
            strict,
            out,
        } => {
            let base: Baseline = serde_json::from_slice(
                &std::fs::read(&baseline)
                    .with_context(|| format!("기준선을 읽지 못함: {}", baseline.display()))?,
            )?;
            let got = measure_all(&m)?;
            if let Some(path) = out {
                save(&path, &got, m.toolchain.clone())?;
            }
            let failed = print_table(&got, Some(&base), threshold);
            if failed.is_empty() {
                println!("통과: 기준선보다 허용치를 넘게 느려진 작업이 없습니다");
                return Ok(());
            }
            if !strict && toolchain_differs(base.toolchain.as_deref(), m.toolchain.as_deref()) {
                let msg = format!(
                    "툴체인이 기준선과 달라 비교는 참고용입니다(기준선: {}, 지금: {}). 느려진 작업: {}",
                    base.toolchain.as_deref().unwrap_or("?"),
                    m.toolchain.as_deref().unwrap_or("?"),
                    failed.join(", ")
                );
                println!("{msg}");
                summary(&format!("\n> {msg}\n"));
                return Ok(());
            }
            bail!("느려진 작업: {}", failed.join(", "))
        }
        Cmd::Scale(args) => scale_report::run(*args),
        Cmd::ScaleLoad { dir } => {
            let r = yangpa_bench::scale::store::cold_load(&dir)?;
            println!("{}", serde_json::to_string(&r)?);
            Ok(())
        }
        Cmd::ScaleCheckpointLoad { dir, mode } => {
            let r = yangpa_bench::scale::checkpoint::cold_start(&dir, mode)?;
            println!("{}", serde_json::to_string(&r)?);
            Ok(())
        }
        Cmd::Time { names, iters } => {
            for s in selected(&names)? {
                let mut w = (s.setup)()?;
                let n = iters.unwrap_or(s.time_iters);
                // 데우기(캐시·지연 초기화).
                w.run(1.min(n))?;
                let t = Instant::now();
                w.run(n.saturating_sub(1).max(1))?;
                let per = t.elapsed() / n.saturating_sub(1).max(1) as u32;
                println!("{:<24} {:>12.3?}/회 ({} 회)", s.name, per, n);
            }
            Ok(())
        }
    }
}

fn selected(names: &[String]) -> Result<Vec<Spec>> {
    if names.is_empty() {
        return Ok(specs());
    }
    names
        .iter()
        .map(|n| find(n).with_context(|| format!("없는 작업: {n}")))
        .collect()
}

/// 둘 다 알려져 있고 다를 때만 참(모르면 같은 것으로 보고 엄격하게 비교).
fn toolchain_differs(base: Option<&str>, now: Option<&str>) -> bool {
    matches!((base, now), (Some(a), Some(b)) if a.trim() != b.trim())
}

fn save(path: &Path, got: &BTreeMap<String, u64>, toolchain: Option<String>) -> Result<()> {
    let b = Baseline {
        note: format!(
            "yangpa-bench measure(valgrind cachegrind, 한 번당 명령 수, profile perf, {} {})",
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
        toolchain,
        instructions: got.clone(),
    };
    let mut bytes = serde_json::to_vec_pretty(&b)?;
    bytes.push(b'\n');
    std::fs::write(path, bytes)?;
    Ok(())
}

fn measure_all(m: &MeasureArgs) -> Result<BTreeMap<String, u64>> {
    if m.n2 <= m.n1 {
        bail!("n2가 n1보다 커야 합니다");
    }
    let exe = std::env::current_exe()?;
    let mut out = BTreeMap::new();
    for s in selected(&m.names)? {
        let a = instructions(&m.valgrind, &exe, s.name, m.n1)?;
        let b = instructions(&m.valgrind, &exe, s.name, m.n2)?;
        let per = b.saturating_sub(a) / (m.n2 - m.n1);
        eprintln!("{}: {} → {} (한 번당 {})", s.name, a, b, per);
        out.insert(s.name.to_string(), per);
    }
    Ok(out)
}

/// `run name n`을 cachegrind로 돌린 전체 명령 수.
fn instructions(valgrind: &str, exe: &Path, name: &str, n: u64) -> Result<u64> {
    let out = Command::new(valgrind)
        .args([
            "--tool=cachegrind",
            "--cache-sim=no",
            "--instr-at-start=no",
            "--cachegrind-out-file=/dev/null",
        ])
        .arg(exe)
        .args(["run", name, &n.to_string()])
        .output()
        .with_context(|| format!("{valgrind}를 실행하지 못함(설치돼 있나요?)"))?;
    let err = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        bail!("{name} 실행 실패:\n{err}");
    }
    parse_irefs(&err).ok_or_else(|| anyhow!("cachegrind 출력에서 I refs를 찾지 못함:\n{err}"))
}

/// cachegrind 클라이언트 요청(valgrind/cachegrind.h, `VG_USERREQ_TOOL_BASE('C','G')`부터).
const CG_START_INSTRUMENTATION: u64 = 0x4347_0000;
const CG_STOP_INSTRUMENTATION: u64 = 0x4347_0001;

/// valgrind 클라이언트 요청(valgrind.h의 amd64 `VALGRIND_DO_CLIENT_REQUEST_EXPR`). valgrind 밖에서는 rdi를 모두
/// 128비트 돌려 제자리로 오는 명령일 뿐이라 아무 일도 하지 않습니다.
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn cachegrind_request(request: u64) {
    let args: [u64; 6] = [request, 0, 0, 0, 0, 0];
    // SAFETY: 위 명령열은 rdi(되돌아옴)·rdx(결과)만 바꾸고, valgrind 아래에서는 rax가 가리키는 인자 배열을 읽기만
    // 합니다(배열은 이 함수가 끝날 때까지 삶).
    unsafe {
        std::arch::asm!(
            "rol rdi, 3",
            "rol rdi, 13",
            "rol rdi, 61",
            "rol rdi, 51",
            "xchg rbx, rbx",
            in("rax") args.as_ptr(),
            inout("rdx") 0u64 => _,
            inout("rdi") 0u64 => _,
        );
    }
}

/// valgrind 명령 수 측정은 리눅스 x86-64에서만 합니다.
#[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
fn cachegrind_request(_request: u64) {}

/// `==123== I refs:      1,234,567` 줄의 수.
fn parse_irefs(stderr: &str) -> Option<u64> {
    stderr.lines().find_map(|l| {
        let (_, rest) = l.split_once("I refs:")?;
        rest.trim().replace(',', "").parse().ok()
    })
}

/// 표를 찍고 허용치를 넘은 작업 이름을 돌려줍니다.
fn print_table(
    got: &BTreeMap<String, u64>,
    base: Option<&Baseline>,
    threshold: Option<f64>,
) -> Vec<String> {
    let mut failed = Vec::new();
    let mut md =
        String::from("| 작업 | 기준선 | 이번 | 변화 | 허용 |\n|---|---:|---:|---:|---:|\n");
    for (name, &now) in got {
        let tol = threshold
            .or_else(|| find(name).map(|s| s.tolerance))
            .unwrap_or(0.10);
        match base.and_then(|b| b.instructions.get(name)) {
            Some(&was) if was > 0 => {
                let change = now as f64 / was as f64 - 1.0;
                let bad = change > tol;
                if bad {
                    failed.push(name.clone());
                }
                println!(
                    "{:<24} {:>14} {:>14} {:>+8.2}% (허용 {:.0}%){}",
                    name,
                    was,
                    now,
                    change * 100.0,
                    tol * 100.0,
                    if bad { "  ← 느려짐" } else { "" }
                );
                md.push_str(&format!(
                    "| {name} | {was} | {now} | {:+.2}% | {:.0}%{} |\n",
                    change * 100.0,
                    tol * 100.0,
                    if bad { " **느려짐**" } else { "" }
                ));
            }
            _ => {
                println!("{name:<24} {:>14} {now:>14}", "-");
                md.push_str(&format!("| {name} | - | {now} | | |\n"));
            }
        }
    }
    summary(&md);
    failed
}

/// GitHub Actions 요약에 덧붙입니다(그 밖에서는 아무것도 안 함).
fn summary(md: &str) {
    if let Some(p) = std::env::var_os("GITHUB_STEP_SUMMARY") {
        let _ = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(p)
            .and_then(|mut f| std::io::Write::write_all(&mut f, md.as_bytes()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cachegrind_summary() {
        let s = "==42== Cachegrind, a high-precision tracing profiler\n==42== \n==42== I refs:        12,345,678\n";
        assert_eq!(parse_irefs(s), Some(12_345_678));
        assert_eq!(parse_irefs("nothing"), None);
    }

    #[test]
    fn toolchain_mismatch_is_advisory_only_when_both_known() {
        assert!(toolchain_differs(
            Some("rustc 1.97.0"),
            Some("rustc 1.99.1")
        ));
        assert!(!toolchain_differs(
            Some("rustc 1.99.1"),
            Some("rustc 1.99.1 ")
        ));
        assert!(!toolchain_differs(None, Some("rustc 1.99.1")));
        assert!(!toolchain_differs(Some("rustc 1.99.1"), None));
    }

    #[test]
    fn ci_toolchain_matches_baseline() {
        // CI(bench.yml)가 고정한 툴체인이 기준선을 잰 툴체인과 같아야 저장된 기준선 비교가 실제로 막습니다
        // (다르면 엄격 모드의 CI가 실패). 고정 판을 올릴 때 기준선도 같이 다시 재게 합니다.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let wf = std::fs::read_to_string(root.join("../../.github/workflows/bench.yml"))
            .expect("bench.yml");
        let pinned = wf
            .lines()
            .find_map(|l| l.trim().strip_prefix("BENCH_TOOLCHAIN:"))
            .map(|v| v.trim().trim_matches('"').to_string())
            .expect("bench.yml의 BENCH_TOOLCHAIN");
        let base: Baseline =
            serde_json::from_slice(&std::fs::read(root.join("baseline.json")).expect("기준선"))
                .expect("기준선 형식");
        let measured = base.toolchain.expect("기준선의 toolchain");
        assert!(
            measured.starts_with(&format!("rustc {pinned} ")),
            "bench.yml의 툴체인 {pinned}과 기준선을 잰 툴체인({measured})이 다름"
        );
    }

    #[test]
    fn every_workload_runs() {
        // 준비·반복이 오류 없이 도는지만(시간은 재지 않음).
        for s in specs() {
            let mut w = (s.setup)().unwrap_or_else(|e| panic!("{} 준비: {e:?}", s.name));
            w.run(2)
                .unwrap_or_else(|e| panic!("{} 반복: {e:?}", s.name));
        }
    }
}
