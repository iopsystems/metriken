#!/usr/bin/env bash
# Run the metriken-query steady-state bench while a Rezolus recorder
# captures system-level metrics (RSS, page faults, ctx switches, syscall
# rates, CPU time). Output is a parquet that `rezolus view` can render.
#
# Prerequisites:
#   1. Rezolus agent running locally — `su -c '/work/rezolus/target/release/rezolus /work/rezolus/config/agent.toml'`
#      (needs root for eBPF). Default endpoint: http://127.0.0.1:4241.
#   2. metriken-query bench built — `cargo build --release --example steady_state_bench` from /work/metriken.
#
# Usage:
#   ./bench-with-rezolus.sh <tag>
#       <tag> is an arbitrary label for the output (e.g. "phase-a", "b1").
#       Outputs land in ./docs/bench/rezolus-<tag>.parquet.
#
# View afterwards:
#   /work/rezolus/target/release/rezolus view metriken-query-sql/scripts/rezolus-<tag>.parquet --listen 127.0.0.1:8080

set -euo pipefail

TAG="${1:-untagged}"
AGENT_URL="${AGENT_URL:-http://127.0.0.1:4241}"
REZOLUS="${REZOLUS:-/work/rezolus/target/release/rezolus}"
WORKSPACE="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BENCH_BIN="$WORKSPACE/target/release/examples/steady_state_bench"
OUT_DIR="$WORKSPACE/metriken-query/docs/bench"
OUT_PARQUET="$OUT_DIR/rezolus-${TAG}.parquet"

if ! curl -fsS --max-time 2 "${AGENT_URL}/metrics" > /dev/null 2>&1; then
    echo "ERROR: Rezolus agent not reachable at ${AGENT_URL}." >&2
    echo "Start it first:  su -c '${REZOLUS} /work/rezolus/config/agent.toml'" >&2
    exit 1
fi

if [[ ! -x "$BENCH_BIN" ]]; then
    echo "ERROR: bench binary missing at ${BENCH_BIN}." >&2
    echo "Build it:  cd ${WORKSPACE} && cargo build --release --example steady_state_bench" >&2
    exit 1
fi

mkdir -p "$OUT_DIR"

# Start the recorder in background. --duration is a ceiling; we kill it
# as soon as the bench exits so the parquet only covers the bench window.
echo "Starting rezolus record → ${OUT_PARQUET}"
"$REZOLUS" record "$AGENT_URL" "$OUT_PARQUET" --duration 1h &
REC_PID=$!

# Brief settle so the first sample lands before the bench starts.
sleep 1

echo "Running bench (tag=${TAG})..."
BENCH_RC=0
"$BENCH_BIN" || BENCH_RC=$?

# Stop the recorder cleanly so it flushes the parquet footer.
echo "Stopping recorder (pid=${REC_PID})"
kill -INT "$REC_PID" 2>/dev/null || true
wait "$REC_PID" 2>/dev/null || true

if [[ -f "$OUT_PARQUET" ]]; then
    SIZE=$(stat -c%s "$OUT_PARQUET")
    echo "Recorded ${SIZE} bytes → ${OUT_PARQUET}"
    echo "View:  ${REZOLUS} view ${OUT_PARQUET} --listen 127.0.0.1:8080"
fi

exit "$BENCH_RC"
