#!/usr/bin/env bash
# Runs the cargo-fuzz targets one after another (one process, one CPU each), SECONDS apiece,
# with their committed seeds and dictionaries; new inputs land in fuzz/corpus/<target>
# (not committed). Prints executions and coverage per target, keeps going after a failure,
# and exits non-zero if any target failed (the reproducer is in fuzz/artifacts/<target>/).
#
#   fuzz/run.sh SECONDS [TARGET...]       FUZZ_TOOLCHAIN=nightly-YYYY-MM-DD to pin
set -euo pipefail
cd "$(dirname "$0")"
secs=${1:?usage: fuzz/run.sh SECONDS [TARGET...]}
shift
toolchain=${FUZZ_TOOLCHAIN:-nightly}
targets=("$@")
if [ ${#targets[@]} -eq 0 ]; then
  mapfile -t targets < <(cargo +"$toolchain" fuzz list)
fi

mkdir -p logs
failed=()
for t in "${targets[@]}"; do
  dirs=("corpus/$t")
  [ -d "seeds/$t" ] && dirs+=("seeds/$t")
  flags=(-max_total_time="$secs" -print_final_stats=1 -timeout=10 -rss_limit_mb=2048)
  [ -f "dict/$t.dict" ] && flags+=(-dict="dict/$t.dict")
  mkdir -p "corpus/$t"
  # -a: debug assertions and overflow checks, so a wrapping add is a finding.
  if cargo +"$toolchain" fuzz run -a "$t" "${dirs[@]}" -- "${flags[@]}" >"logs/$t.log" 2>&1; then
    status=ok
  else
    status=FAILED
    failed+=("$t")
  fi
  execs=$(grep -o 'stat::number_of_executed_units: [0-9]*' "logs/$t.log" | grep -o '[0-9]*$' || true)
  cov=$(grep -oE 'cov: [0-9]+ ft: [0-9]+' "logs/$t.log" | tail -1 || true)
  printf '%-20s %-6s execs=%-10s %s\n' "$t" "$status" "${execs:-?}" "$cov"
done

if [ ${#failed[@]} -gt 0 ]; then
  for t in "${failed[@]}"; do
    echo "::error title=fuzz target $t failed::$(grep -m1 -E 'panicked at|ERROR: libFuzzer' "logs/$t.log" || echo "see fuzz/logs/$t.log")"
    grep -A12 -m1 -E 'panicked at|ERROR: libFuzzer' "logs/$t.log" || true
  done
  exit 1
fi
