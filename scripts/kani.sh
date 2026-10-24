#!/usr/bin/env bash
# Run the Kani proof harnesses one at a time, each with a hard memory cap and
# a timeout, so a runaway proof cannot exhaust the machine's memory.
#
#   scripts/kani.sh                 # all harnesses
#   scripts/kani.sh atom_ frames_   # harnesses whose name contains any pattern
#
# Environment: KANI_MEM (default 4G), KANI_TIMEOUT seconds (default 30),
# KANI_LOGS (default target/kani-logs).
set -u
cd "$(dirname "$0")/.."
MEM=${KANI_MEM:-4G}
TIMEOUT=${KANI_TIMEOUT:-30}
LOGS=${KANI_LOGS:-target/kani-logs}
# Reclaim pressure starts at 3/4 of the cap, before the hard kill.
HIGH=$(( $(numfmt --from=iec "$MEM") * 3 / 4 ))
# Compiling the crate for Kani is memory hungry too: fewer parallel jobs.
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}
mkdir -p "$LOGS"

mapfile -t ALL < <(grep -A3 '#\[kani::proof\]' src/verification.rs | sed -n 's/^fn \([a-z0-9_]*\).*/\1/p')
if [ $# -gt 0 ]; then
    HARNESSES=()
    for h in "${ALL[@]}"; do for p in "$@"; do [[ $h == *"$p"* ]] && { HARNESSES+=("$h"); break; }; done; done
else
    HARNESSES=("${ALL[@]}")
fi

# Build once, uncapped (compilation is not the memory problem).
cargo kani --only-codegen >"$LOGS/codegen.log" 2>&1 || { echo "codegen failed, see $LOGS/codegen.log"; exit 1; }

capped() {
    if systemd-run --user --scope -q -p MemoryMax=1M true >/dev/null 2>&1; then
        systemd-run --user --scope -q -p MemoryHigh="$HIGH" -p MemoryMax="$MEM" -p MemorySwapMax=0 -- "$@"
    else
        # No systemd user session: cap the address space instead.
        bytes=$(numfmt --from=iec "$MEM")
        prlimit --as="$bytes" -- "$@"
    fi
}

fail=0
for h in "${HARNESSES[@]}"; do
    start=$(date +%s)
    capped timeout "$TIMEOUT" cargo kani --harness "verification::$h" --exact --output-format terse >"$LOGS/$h.log" 2>&1
    rc=$?
    secs=$(( $(date +%s) - start ))
    if grep -q "VERIFICATION:- SUCCESSFUL" "$LOGS/$h.log"; then
        status="OK"
    elif grep -q "VERIFICATION:- FAILED" "$LOGS/$h.log"; then
        status="FAILED"; fail=1
    elif [ $rc -eq 124 ]; then
        status="TIMEOUT (${TIMEOUT}s)"; fail=1
    else
        status="KILLED (memory/time limit, exit $rc)"; fail=1
    fi
    printf '%-45s %-10s %4ss\n' "$h" "$status" "$secs" | tee -a "$LOGS/summary.txt"
done
exit $fail
