#!/usr/bin/env bash
# run_all.sh - sequential runner for the real-CLI e2e scenarios.
#
# Usage: scrtips/e2e_scenarios/run_all.sh [scenario.sh ...]
#   (default: every scenario_*.sh in this directory)
# Exit code = number of failed scenarios. Env: RDB_BIN, RDB_E2E_TOKEN,
# RDB_E2E_PORT_BASE, RDB_E2E_KEEP_WORKDIR pass through to env.sh.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$HERE/../.."   # repo root: scenarios resolve the release binary from here

scenarios=()
if [ $# -gt 0 ]; then
    for s in "$@"; do
        # accept bare names from any cwd, with or without .sh
        if [ ! -f "$s" ]; then
            s="$HERE/$s"
            [ -f "$s" ] || s="$s.sh"
        fi
        scenarios+=("$s")
    done
else
    for f in "$HERE"/scenario_*.sh; do
        [ -f "$f" ] && scenarios+=("$f")
    done
fi

pass=0
fail=0
failed_names=()
echo "== rdb real-CLI e2e: $(date -u '+%Y-%m-%dT%H:%M:%SZ') =="
echo "binary: ${RDB_BIN:-$HERE/../../target/release/rdb}"
if [ -f target/release/rdb ]; then
    echo "git:    $(git rev-parse --short HEAD 2>/dev/null || echo 'n/a')  bin mtime: $(date -r target/release/rdb '+%s')"
fi

# W0.4 pre-flight: the binary under test must carry the LIFO-disabled
# startup banner (compile-time cfg-selected literal; the DANGER variant
# means the binary can freeze under load -- reject it before any run).
RDB_BIN_PATH="${RDB_BIN:-$HERE/../../target/release/rdb}"
if ! LC_ALL=C grep -aq 'tokio LIFO slot: disabled' "$RDB_BIN_PATH"; then
    echo "FATAL: $RDB_BIN_PATH lacks the 'tokio LIFO slot: disabled' banner"
    echo "       (built without --cfg tokio_unstable; rebuild via .cargo/config.toml)" >&2
    exit 1
fi
for s in "${scenarios[@]}"; do
    name="$(basename "$s")"
    echo "---- $name ----"
    if bash "$s" >"/tmp/rdb_e2e_${name%.sh}.out" 2>&1; then
        echo "PASS  $name"
        pass=$((pass + 1))
    else
        echo "FAIL  $name  (log: /tmp/rdb_e2e_${name%.sh}.out)"
        tail -n 25 "/tmp/rdb_e2e_${name%.sh}.out"
        fail=$((fail + 1))
        failed_names+=("$name")
    fi
done

echo "== summary: $pass pass, $fail fail =="
if [ "$fail" -ne 0 ]; then
    printf 'failed: %s\n' "${failed_names[@]}"
    # GitHub annotations are readable via the anonymous check-runs API
    # even when log downloads are not; surface each failure's tail there.
    if [ "${GITHUB_ACTIONS:-}" = "true" ]; then
        for name in "${failed_names[@]}"; do
            short="${name#scenario_}"; short="${short%.sh}"
            tail_txt="$(tail -n 8 "/tmp/rdb_e2e_${name%.sh}.out" 2>/dev/null | tr '\n' '|' | tail -c 300)"
            printf '::error title=e2e-%s::%s\n' "$short" "${tail_txt:-no-log}"
        done
    fi
fi
exit "$fail"
