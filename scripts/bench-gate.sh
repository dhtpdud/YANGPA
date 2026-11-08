#!/usr/bin/env bash
# 명령 수 회귀 검사(.github/workflows/bench.yml, docs/performance.md).
#
# 사용: scripts/bench-gate.sh [기준 커밋]
#
# - 기준 커밋을 주면(PR) 그 커밋의 yangpa-bench를 같은 러너·같은 툴체인으로 재서 기준선으로 씁니다. 명령 수는
#   컴파일러 판에 따라 달라지므로 이 비교가 가장 정확합니다. 기준 커밋에 yangpa-bench가 없으면 저장된 기준선을 씁니다.
# - 기준 커밋이 없으면 저장된 기준선(crates/yangpa-bench/baseline.json)과 비교합니다. 기준선을 잰 툴체인과
#   다르면 비교는 참고용으로만 찍습니다. YANGPA_BENCH_STRICT=1(CI)이면 그래도 실패합니다: CI는 툴체인을 기준선에
#   맞춰 고정하므로, 어긋났다면 고정 판을 올리고 기준선을 다시 재지 않은 것입니다.
# - 결과: $BENCH_OUT(기본 target/bench)/head.json(이번 측정, 기준선을 새로 쓸 때 그대로 복사), base.json.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${BENCH_OUT:-$ROOT/target/bench}"
mkdir -p "$OUT"
YANGPA_BENCH_TOOLCHAIN="${YANGPA_BENCH_TOOLCHAIN:-$(rustc --version)}"
export YANGPA_BENCH_TOOLCHAIN
BIN="$ROOT/target/perf/yangpa-bench"
BASELINE="$ROOT/crates/yangpa-bench/baseline.json"
BASE_REF="${1:-}"

if [[ -n "$BASE_REF" ]]; then
  BASE_DIR="$OUT/base-src"
  rm -rf "$BASE_DIR"
  git -C "$ROOT" worktree prune
  git -C "$ROOT" worktree add --detach "$BASE_DIR" "$BASE_REF"
  if [[ -f "$BASE_DIR/crates/yangpa-bench/Cargo.toml" ]]; then
    echo "기준 커밋 $BASE_REF 측정"
    # 같은 대상 경로에 빌드해 의존성 빌드를 나눠 씁니다. 재고 나서 바로 머리 쪽을 빌드하므로 덮여도 됩니다.
    cargo build --profile perf -p yangpa-bench --manifest-path "$BASE_DIR/Cargo.toml" --target-dir "$ROOT/target"
    "$BIN" measure --out "$OUT/base.json"
    BASELINE="$OUT/base.json"
  else
    echo "기준 커밋에 yangpa-bench가 없어 저장된 기준선과 비교합니다"
  fi
  # 기준 쪽 소스를 지워 두면 머리 쪽 빌드가 작업 공간 크레이트를 반드시 다시 빌드합니다.
  git -C "$ROOT" worktree remove --force "$BASE_DIR"
fi

GATE_ARGS=(--baseline "$BASELINE" --out "$OUT/head.json")
if [[ "${YANGPA_BENCH_STRICT:-}" == "1" ]]; then
  GATE_ARGS+=(--strict)
fi

cargo build --profile perf -p yangpa-bench --manifest-path "$ROOT/Cargo.toml"
"$BIN" gate "${GATE_ARGS[@]}"
