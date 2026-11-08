//! `cargo bench -p yangpa-bench [-- 작업이름…]`: 작업마다 한 번당 벽시계 시간을 찍습니다.
//!
//! 사람이 보는 숫자입니다. 기계마다 달라 CI 회귀 검사에는 쓰지 않고, 검사는 명령 수로 합니다
//! (`yangpa-bench gate`, docs/performance.md).

use std::time::Instant;

fn main() -> anyhow::Result<()> {
    // `cargo bench`가 넘기는 `--bench` 같은 인자는 건너뜁니다.
    let names: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .collect();
    for s in yangpa_bench::specs() {
        if !names.is_empty() && !names.iter().any(|n| n == s.name) {
            continue;
        }
        let mut w = (s.setup)()?;
        w.run(1)?;
        let n = s.time_iters.saturating_sub(1).max(1);
        let t = Instant::now();
        w.run(n)?;
        let per = t.elapsed() / n as u32;
        println!("{:<24} {:>12.3?}/회 ({} 회)  {}", s.name, per, n, s.about);
    }
    Ok(())
}
