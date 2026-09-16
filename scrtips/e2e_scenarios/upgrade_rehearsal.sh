#!/usr/bin/env bash
# upgrade_rehearsal.sh - W2.2 UPGRADE REHEARSAL: same-batch (co-upgrade)
# of a 3-node cluster across a catalog-shape change.
#
# COMPAT.md gates ROLLING upgrades as unsafe whenever the raft catalog
# JSON changes shape; this iteration turned TableSchema.pk into an array
# (composite pk, W2.1) and added the DECIMAL variant to SqlType (W2.0),
# so all nodes must move to the new binary inside one stop-the-world
# window. This rehearsal proves that path end to end:
#   phase 0  build BOTH binaries: OLD = detached worktree at
#            RDB_UPGRADE_OLD_COMMIT (default c22ff37, the last commit
#            before this iteration), NEW = the current tree.
#   phase A  OLD binary brings the cluster up, seeds the old feature
#            surface (single-col BIGINT pk + VARCHAR/DATE + secondary +
#            unique index, StarRocks single-col PRIMARY KEY model,
#            AUTO_INCREMENT; UPDATE/DELETE for multi-versions and
#            tombstones), snapshots counts/rows/DESCRIBE/SHOW INDEX,
#            then SIGTERM-all and waits for exit.
#   phase B  NEW binary restarts the SAME data dirs: health, snapshot
#            equality, index point lookup, AUTO_INCREMENT continuation,
#            writes on old tables (1062, explicit txn), and the NEW
#            capabilities (DECIMAL(10,2)+index, composite pk table,
#            StarRocks multi-column PRIMARY KEY model).
#   phase C  kill -9 the whole new cluster, restart, re-verify the
#            post-write snapshot (durable ts floor; the one-time boot
#            floor scan must NOT run a second time).
#
# The worktree (.upgrade-old) and every scratch dir are removed on exit;
# repeat runs are idempotent. Env: RDB_UPGRADE_OLD_COMMIT plus the
# RDB_E2E_* knobs env.sh documents. Port band defaults to 32900 (the
# scenario_* suite uses 32700) so both may run side by side.
set -uo pipefail

UP_OLD_COMMIT="${RDB_UPGRADE_OLD_COMMIT:-c22ff37}"
export RDB_E2E_PORT_BASE="${RDB_E2E_PORT_BASE:-32900}"
E2E_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# shellcheck source=env.sh
. "$E2E_ROOT/scrtips/e2e_scenarios/env.sh"

NEW_BIN="$E2E_ROOT/target/release/rdb"
OLD_WT="$E2E_ROOT/.upgrade-old"
OLD_BIN="$OLD_WT/target/release/rdb"
SCENARIO="upgrade_rehearsal"
SNAP=""    # active snapshot dir (snapA = old-binary truth, snapB = post-write)
LEADER=""  # raft leader idx, set by up_start_all after each restart
SQL_ERR_FILE=""

# ---- SQL wrappers + local assertions (scenario_mysql_orders.sh shapes) --
sql_mysql () { # <idx> <sql> -> SQL_OUT / SQL_ERR_TEXT / SQL_RC
    SQL_OUT="$(mysql --no-defaults -h "$E2E_HOST" -P "$(node_mysql "$1")" -u root \
        --connect-timeout=5 -B -N --raw -e "$2" 2>"$SQL_ERR_FILE")"
    SQL_RC=$?
    SQL_ERR_TEXT="$(cat "$SQL_ERR_FILE")"
}
norm_rows () { printf '%s' "$SQL_OUT" | tr '\t' ',' | paste -sd';' -; }
norm_sorted () { norm_rows | tr ';' '\n' | LC_ALL=C sort | paste -sd';' -; }

assert_ok () {
    if [ "$SQL_RC" -eq 0 ]; then echo "ok   - $1"
    else E2E_FAILS=$((E2E_FAILS + 1)); echo "FAIL: $1: rc=$SQL_RC err=[$SQL_ERR_TEXT]" >&2; fi
}
assert_fails () {
    if [ "$SQL_RC" -ne 0 ]; then echo "ok   - $1"
    else E2E_FAILS=$((E2E_FAILS + 1)); echo "FAIL: $1: expected failure, rc=0 out=[$SQL_OUT]" >&2; fi
}
assert_err_contains () {
    if [ "$SQL_RC" -eq 0 ]; then E2E_FAILS=$((E2E_FAILS + 1)); echo "FAIL: $1: expected failure, rc=0" >&2
    elif [[ "$SQL_ERR_TEXT" == *"$2"* ]]; then echo "ok   - $1"
    else E2E_FAILS=$((E2E_FAILS + 1)); echo "FAIL: $1: [$2] not in [$SQL_ERR_TEXT]" >&2; fi
}
assert_dup_1062 () {
    assert_fails "$1 (statement rejected)"
    assert_err_contains "$1 (errno 1062)" "1062"
}
assert_eventually () { # desc cmd args...
    local desc=$1; shift
    if "$@"; then assert_eq "$desc" "ok" "ok"; else assert_eq "$desc" "ok" "timeout"; fi
}
ddl_exec () { # <idx> <sql>: retry while stderr blames leadership
    local tries=0
    while :; do
        sql_mysql "$1" "$2"
        if [ "$SQL_RC" -eq 0 ] || [[ "${SQL_ERR_TEXT#*leader}" == "$SQL_ERR_TEXT" ]]; then
            return 0
        fi
        tries=$((tries + 1)); [ "$tries" -ge 10 ] && return 0
        sleep 1
    done
}
poll_has () { # idx sql needle timeout -> POLL_OUT on success
    local deadline=$((SECONDS + $4))
    POLL_OUT=""
    while :; do
        sql_mysql "$1" "$2"
        if [ "$SQL_RC" -eq 0 ] && [[ "$SQL_OUT" == *"$3"* ]]; then POLL_OUT="$SQL_OUT"; return 0; fi
        [ "$SECONDS" -ge "$deadline" ] && return 1
        sleep 1
    done
}

# ---- snapshots: phase A stores the old-binary truth, later phases diff --
snap_take () { # <name> <idx> <sql> [sorted]
    sql_mysql "$2" "$3"
    assert_ok "snapshot $1 ([$3])"
    if [ "${4:-}" = sorted ]; then norm_sorted >"$SNAP/$1"; else norm_rows >"$SNAP/$1"; fi
}
snap_eq () { # <name> <desc> <idx> <sql> [sorted]
    sql_mysql "$3" "$4"
    if [ "$SQL_RC" -ne 0 ]; then
        E2E_FAILS=$((E2E_FAILS + 1)); echo "FAIL: $2: rc=$SQL_RC err=[$SQL_ERR_TEXT]" >&2; return
    fi
    local got
    if [ "${5:-}" = sorted ]; then got="$(norm_sorted)"; else got="$(norm_rows)"; fi
    assert_eq "$2" "$(cat "$SNAP/$1")" "$got"
}

# ---- lifecycle beyond env.sh --------------------------------------------
up_spawn () { # <idx>: start $RDB_BIN on the EXISTING data dir (no bootstrap/join)
    RAFT_BOOTSTRAP= RAFT_JOIN_ADDR= "$RDB_BIN" -config "$E2E_WORKDIR/conf_node$1.yaml" \
        >>"$E2E_WORKDIR/node$1.log" 2>&1 &
    E2E_PIDS[$1]=$!
}
up_start_all () { # restart every node + wait pings; prints the leader idx
    local i
    [ -f "$RDB_BIN" ] || { echo "FATAL: $RDB_BIN missing" >&2; return 1; }
    for i in 0 1 2; do up_spawn "$i"; done
    for i in 0 1 2; do
        _e2e_wait_ping "$i" || { echo "FATAL: node$i RESP not ready" >&2; return 1; }
    done
    e2e_find_leader || { echo "FATAL: no leader after restart" >&2; return 1; }
}
up_stop_cluster () { # <SIG>: signal every node, then reap it
    local pid
    for pid in "${E2E_PIDS[@]:-}"; do [ -n "$pid" ] && kill -"$1" "$pid" 2>/dev/null; done
    for pid in "${E2E_PIDS[@]:-}"; do [ -n "$pid" ] && wait "$pid" 2>/dev/null; done
}
up_assert_all_dead () {
    local pid alive=0
    for pid in "${E2E_PIDS[@]:-}"; do
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then alive=1; fi
    done
    assert_eq "$1" "0" "$alive"
    E2E_PIDS=()
}
up_floor_scans () { grep -h 'boot floor scan recovered' "$E2E_WORKDIR"/node*.log 2>/dev/null | wc -l; }
up_cleanup () {
    e2e_teardown
    if [ "${RDB_E2E_KEEP_WORKDIR:-${E2E_KEEP_WORKDIR:-0}}" != "1" ]; then
        git -C "$E2E_ROOT" worktree remove --force "$OLD_WT" >/dev/null 2>&1 || true
    fi
}

# ---- phase 0: both binaries ---------------------------------------------
phase0_build () {
    # reuse a kept worktree only when it still pins the OLD commit
    if [ -e "$OLD_WT/.git" ] &&
        [ "$(git -C "$OLD_WT" rev-parse HEAD 2>/dev/null)" != \
          "$(git -C "$E2E_ROOT" rev-parse "$UP_OLD_COMMIT" 2>/dev/null)" ]; then
        git -C "$E2E_ROOT" worktree remove --force "$OLD_WT" >/dev/null 2>&1 || true
    fi
    if [ ! -e "$OLD_WT/.git" ]; then
        git -C "$E2E_ROOT" worktree add --detach "$OLD_WT" "$UP_OLD_COMMIT" >/dev/null ||
            { echo "FATAL: worktree add $OLD_WT $UP_OLD_COMMIT failed" >&2; return 1; }
    fi
    echo "== building OLD binary ($UP_OLD_COMMIT) =="
    (cd "$OLD_WT" && cargo build --release) >/tmp/rdb_upgrade_old_build.log 2>&1 ||
        { echo "FATAL: old build failed:" >&2; tail -n 5 /tmp/rdb_upgrade_old_build.log >&2; return 1; }
    echo "== building NEW binary ($(git -C "$E2E_ROOT" rev-parse --short HEAD)) =="
    (cd "$E2E_ROOT" && cargo build --release) >/tmp/rdb_upgrade_new_build.log 2>&1 ||
        { echo "FATAL: new build failed:" >&2; tail -n 5 /tmp/rdb_upgrade_new_build.log >&2; return 1; }
    local b
    for b in "$OLD_BIN" "$NEW_BIN"; do
        LC_ALL=C grep -aq 'tokio LIFO slot: disabled' "$b" ||
            { echo "FATAL: $b lacks the LIFO-disabled banner" >&2; return 1; }
    done
}

# ---- phase A: seed on the OLD binary, snapshot, stop the world ----------
phaseA_seed () {
    RDB_BIN="$OLD_BIN"
    e2e_start_cluster >/dev/null || { echo "FATAL: old-binary cluster did not come up" >&2; return 1; }
    echo "old-binary cluster up: resp $(node_resp 0)/$(node_resp 1)/$(node_resp 2)"
    assert_eventually "old: mysql wire answers SELECT 1" poll_has 0 "SELECT 1" "1" 60

    sql_mysql 0 "CREATE TABLE up_nodec (id BIGINT PRIMARY KEY, v DECIMAL(10,2))"
    assert_fails "old binary rejects DECIMAL DDL (baseline for the new capability)"
    sql_mysql 0 "CREATE TABLE up_nocpk (a BIGINT, b BIGINT, v INT, PRIMARY KEY(a, b))"
    assert_fails "old binary rejects composite PRIMARY KEY (baseline)"

    ddl_exec 0 "CREATE TABLE up_orders (id BIGINT PRIMARY KEY, user_id BIGINT, note VARCHAR(64), created DATE)"
    assert_ok "CREATE up_orders (single-col BIGINT pk + VARCHAR/DATE)"
    ddl_exec 0 "CREATE UNIQUE INDEX uk_user ON up_orders (user_id)"
    assert_ok "CREATE UNIQUE INDEX uk_user (user_id)"
    ddl_exec 0 "CREATE INDEX idx_note ON up_orders (note)"
    assert_ok "CREATE INDEX idx_note (note)"
    ddl_exec 0 "CREATE TABLE up_sr (id BIGINT NOT NULL, name VARCHAR(64) NULL, amount INT NULL) PRIMARY KEY(id) DISTRIBUTED BY HASH(id) BUCKETS 3"
    assert_ok "CREATE up_sr (StarRocks single-col PRIMARY KEY model)"
    ddl_exec 0 "CREATE TABLE up_ai (id BIGINT AUTO_INCREMENT PRIMARY KEY, note VARCHAR(64))"
    assert_ok "CREATE up_ai (AUTO_INCREMENT)"
    assert_eventually "old: catalog spread reaches node1" poll_has 1 "SHOW TABLES" "up_ai" 30

    local i vals="" d
    for i in $(seq 0 29); do
        d="2026-0$((1 + i % 9))-15"
        vals+="($((1000 + i)), $((500 + i)), 'note$i', '$d'),"
    done
    sql_mysql 0 "INSERT INTO up_orders (id, user_id, note, created) VALUES ${vals%,}"
    assert_ok "seed 30 up_orders rows (ids 1000..1029)"
    sql_mysql 0 "UPDATE up_orders SET note = 'bulk-upd' WHERE id >= 1000 AND id <= 1007"
    assert_ok "bulk UPDATE 8 rows (multi-versions)"
    sql_mysql 1 "UPDATE up_orders SET note = 'solo-upd' WHERE id = 1010"
    assert_ok "single-row UPDATE via node1 (cross-node)"
    sql_mysql 2 "DELETE FROM up_orders WHERE id >= 1025 AND id <= 1029"
    assert_ok "DELETE 5 rows via node2 (tombstones)"
    vals=""
    for i in $(seq 1 10); do vals+="($i, 'name$i', $((10 * i))),"; done
    sql_mysql 0 "INSERT INTO up_sr (id, name, amount) VALUES ${vals%,}"
    assert_ok "seed 10 up_sr rows"
    sql_mysql 0 "INSERT INTO up_sr (id, name, amount) VALUES (1, 'name1-new', 111), (2, 'name2-new', 222), (3, 'name3-new', 333)"
    assert_ok "upsert 3 up_sr pks (pk-model replace)"
    vals=""
    for i in $(seq 1 12); do vals+="('ai-$i'),"; done
    sql_mysql 0 "INSERT INTO up_ai (note) VALUES ${vals%,}"
    assert_ok "12 auto inserts (ids 1..12; RESERVE_BATCH=64 persists next=65)"
    sql_mysql 0 "INSERT INTO up_ai (id, note) VALUES (50, 'explicit-50')"
    assert_ok "explicit id=50 (below the reserved floor: counter stays 65)"

    SNAP="$E2E_WORKDIR/snapA"; mkdir -p "$SNAP"
    assert_eventually "old: writes settled (25 orders rows)" poll_has 2 "SELECT COUNT(*) FROM up_orders" "25" 30
    # NB: snapshots use SHOW COLUMNS (not DESCRIBE -- the DESCRIBE alias
    # itself only landed with W2.0, so the OLD binary would reject it).
    snap_take tables 0 "SHOW TABLES" sorted
    snap_take orders_rows 2 "SELECT id, user_id, note, created FROM up_orders ORDER BY id"
    snap_take orders_cnt 1 "SELECT COUNT(*) FROM up_orders"
    snap_take orders_desc 0 "SHOW COLUMNS FROM up_orders"
    snap_take orders_idx 0 "SHOW INDEX FROM up_orders"
    snap_take note_point 1 "SELECT id FROM up_orders WHERE note = 'bulk-upd' ORDER BY id"
    snap_take sr_rows 2 "SELECT id, name, amount FROM up_sr ORDER BY id"
    snap_take sr_desc 0 "SHOW COLUMNS FROM up_sr"
    snap_take ai_ids 1 "SELECT id FROM up_ai ORDER BY id"

    echo "== stop-the-world upgrade window: SIGTERM all nodes =="
    up_stop_cluster TERM
    up_assert_all_dead "old cluster: every node exited on SIGTERM"
}

# ---- phase B: NEW binary on the SAME dirs -------------------------------
phaseB_verify () {
    RDB_BIN="$NEW_BIN"
    echo "== same-batch swap: restarting all nodes on the NEW binary =="
    LEADER="$(up_start_all)" || return 1
    assert_eventually "new: mysql wire answers SELECT 1" poll_has 0 "SELECT 1" "1" 60
    assert_eq "upgrade boot: one-time floor scan recovered the pre-floor data" "yes" \
        "$([ "$(up_floor_scans)" -ge 1 ] && echo yes || echo no)"

    snap_eq tables "catalog: SHOW TABLES identical on node0" 0 "SHOW TABLES" sorted
    snap_eq tables "catalog: SHOW TABLES identical on node2" 2 "SHOW TABLES" sorted
    snap_eq orders_cnt "old rows: COUNT(*) unchanged" 0 "SELECT COUNT(*) FROM up_orders"
    snap_eq orders_rows "old rows: every row/column matches (via node2)" 2 \
        "SELECT id, user_id, note, created FROM up_orders ORDER BY id"
    snap_eq orders_desc "old rows: SHOW COLUMNS up_orders identical" 0 "SHOW COLUMNS FROM up_orders"
    snap_eq orders_idx "old rows: SHOW INDEX identical (index catalog survived)" 0 "SHOW INDEX FROM up_orders"
    snap_eq note_point "old rows: secondary-index point lookup (idx_note) correct" 1 \
        "SELECT id FROM up_orders WHERE note = 'bulk-upd' ORDER BY id"
    snap_eq sr_rows "StarRocks pk-model rows unchanged" 2 "SELECT id, name, amount FROM up_sr ORDER BY id"
    snap_eq sr_desc "SHOW COLUMNS up_sr identical (key_model + distribution fields)" 0 "SHOW COLUMNS FROM up_sr"
    snap_eq ai_ids "AUTO_INCREMENT rows unchanged" 1 "SELECT id FROM up_ai ORDER BY id"

    # AUTO_INCREMENT is a leader-only catalog write (same rule as DDL,
    # tests/auto_increment_e2e.rs "allocation must be leader-only");
    # route via the leader and keep a follower-refusal check so the
    # rehearsal proves the rule SURVIVED the upgrade.
    sql_mysql "$LEADER" "INSERT INTO up_ai (note) VALUES ('post-up-1')"
    assert_ok "auto insert after the upgrade (leader node$LEADER)"
    sql_mysql "$LEADER" "INSERT INTO up_ai (note) VALUES ('post-up-2')"
    assert_ok "second auto insert (leader node$LEADER)"
    sql_mysql 0 "SELECT COUNT(*), COUNT(DISTINCT id), MAX(id) FROM up_ai"
    assert_eq "counter carried 65 across the upgrade: ids 65,129 (reservation gaps, no clash)" \
        "15,15,129" "$(norm_rows)"
    sql_mysql "$(( (LEADER + 1) % 3 ))" "INSERT INTO up_ai (note) VALUES ('follower-try')"
    assert_err_contains "follower still refuses AUTO_INCREMENT allocation (leader-only rule kept)" \
        "requires the raft leader"

    sql_mysql 2 "INSERT INTO up_orders (id, user_id, note, created) VALUES (2000, 900, 'new-row', '2026-09-16')"
    assert_ok "INSERT into the old table via node2"
    assert_eventually "new row visible cross-node" poll_has 0 "SELECT COUNT(*) FROM up_orders" "26" 30
    sql_mysql 1 "UPDATE up_orders SET note = 'after-upgrade' WHERE id = 1000"
    assert_ok "UPDATE an old row (newest version wins)"
    assert_eventually "updated value visible via node2" poll_has 2 \
        "SELECT note FROM up_orders WHERE id = 1000" "after-upgrade" 30
    sql_mysql 0 "DELETE FROM up_orders WHERE id = 2000"
    assert_ok "DELETE the row again (fresh tombstone)"
    sql_mysql 0 "SELECT COUNT(*) FROM up_orders"
    assert_eq "count back to 25" "25" "$SQL_OUT"
    sql_mysql 0 "INSERT INTO up_orders (id, user_id, note, created) VALUES (2001, 500, 'dup-user', '2026-09-16')"
    assert_dup_1062 "unique index uk_user still enforced (1062)"
    sql_mysql 1 "BEGIN"
    sql_mysql 1 "INSERT INTO up_orders (id, user_id, note, created) VALUES (2002, 902, 'txn-row', '2026-09-16')"
    sql_mysql 1 "COMMIT"
    assert_ok "explicit txn: staged INSERT + COMMIT"
    assert_eventually "committed txn row visible from node2" poll_has 2 \
        "SELECT COUNT(*) FROM up_orders" "26" 30
    sql_mysql 1 "BEGIN; INSERT INTO up_orders (id, user_id, note, created) VALUES (2003, 903, 'rb-row', '2026-09-16'); ROLLBACK"
    assert_ok "explicit txn: ROLLBACK of a staged INSERT"
    sql_mysql 0 "SELECT COUNT(*) FROM up_orders"
    assert_eq "rolled-back row never visible" "26" "$SQL_OUT"

    ddl_exec 0 "CREATE TABLE up_dec (id BIGINT PRIMARY KEY, amount DECIMAL(10,2), note VARCHAR(64))"
    assert_ok "NEW: CREATE up_dec DECIMAL(10,2)"
    ddl_exec 0 "CREATE INDEX idx_amt ON up_dec (amount)"
    assert_ok "NEW: CREATE INDEX on the decimal column"
    sql_mysql 0 "SELECT 0.1 + 0.2"
    assert_eq "NEW: decimal literals add exactly (0.1+0.2)" "0.3" "$SQL_OUT"
    sql_mysql 1 "INSERT INTO up_dec (id, amount, note) VALUES (1, 0.1, 'a'), (2, 0.2, 'b'), (3, 1.005, 'c')"
    assert_ok "NEW: seed up_dec (half-up rounding case included)"
    assert_eventually "NEW: decimal rows visible cross-node" poll_has 2 "SELECT COUNT(*) FROM up_dec" "3" 30
    sql_mysql 2 "SELECT amount FROM up_dec ORDER BY id"
    assert_eq "NEW: stored decimals exact at scale 2" "0.10;0.20;1.01" "$(norm_rows)"
    sql_mysql 0 "SELECT id FROM up_dec WHERE amount = 1.010"
    assert_eq "NEW: decimal secondary-index point lookup (cross-scale literal)" "3" "$SQL_OUT"
    sql_mysql 1 "SELECT SUM(amount) FROM up_dec"
    assert_eq "NEW: SUM exact at column scale (1.31)" "1.31" "$SQL_OUT"

    ddl_exec 0 "CREATE TABLE up_item (a BIGINT, b VARCHAR(32), v INT, PRIMARY KEY(a, b))"
    assert_ok "NEW: CREATE up_item (composite pk)"
    assert_eventually "NEW: up_item catalog spread" poll_has 1 "SHOW TABLES" "up_item" 30
    sql_mysql 0 "INSERT INTO up_item (a, b, v) VALUES (0, 'k0', 100), (0, 'k1', 101), (1, 'k2', 102), (1, 'k0', 103), (2, 'k3', 104), (2, 'k1', 105)"
    assert_ok "NEW: seed composite-pk rows (a repeats: only the tuple is unique)"
    sql_mysql 1 "INSERT INTO up_item (a, b, v) VALUES (0, 'k0', 999)"
    assert_ok "NEW: composite upsert via node1"
    sql_mysql 2 "SELECT v FROM up_item WHERE a = 0 AND b = 'k0'"
    assert_eq "NEW: full-tuple point lookup sees the upserted value" "999" "$SQL_OUT"
    sql_mysql 0 "SELECT COUNT(*) FROM up_item"
    assert_eq "NEW: upsert replaced the tuple (count stable at 6)" "6" "$SQL_OUT"
    sql_mysql 0 "SHOW COLUMNS FROM up_item"
    assert_contains "NEW: both pk columns flagged PRI" "b,varchar,NO,PRI" "$(norm_rows)"

    ddl_exec 0 "CREATE TABLE up_mpk (k1 BIGINT NOT NULL, k2 VARCHAR(32) NOT NULL, v DECIMAL(10,2) NULL) PRIMARY KEY(k1, k2) DISTRIBUTED BY HASH(k1, k2) BUCKETS 3"
    assert_ok "NEW: CREATE up_mpk (StarRocks multi-column PRIMARY KEY)"
    sql_mysql 0 "INSERT INTO up_mpk (k1, k2, v) VALUES (1, 'a', 0.1), (1, 'b', 0.2), (2, 'c', 0.1)"
    assert_ok "NEW: seed up_mpk"
    # upsert via the LEADER: a follower coordinator hitting a just-
    # committed pk can fail-fast 1213 (cross-coordinator ts ordering,
    # the documented M2-era caveat in COMPAT.md -- not upgrade-related)
    sql_mysql "$LEADER" "INSERT INTO up_mpk (k1, k2, v) VALUES (1, 'a', 0.3)"
    assert_ok "NEW: multi-column pk upsert via the leader"
    assert_eventually "NEW: pk-model upsert deduped on the full tuple" poll_has 2 \
        "SELECT COUNT(*) FROM up_mpk" "3" 30
    sql_mysql 2 "SELECT v FROM up_mpk WHERE k1 = 1 AND k2 = 'a'"
    assert_eq "NEW: upserted tuple carries the new decimal v (0.30)" "0.30" "$SQL_OUT"
    sql_mysql 0 "SELECT SUM(v) FROM up_mpk"
    assert_eq "NEW: exact decimal SUM over the pk model (0.60)" "0.60" "$SQL_OUT"
}

# ---- phase C: crash the new cluster, restart, re-verify -----------------
phaseC_restart () {
    SNAP="$E2E_WORKDIR/snapB"; mkdir -p "$SNAP"
    snap_take tables 0 "SHOW TABLES" sorted
    snap_take orders_rows 0 "SELECT id, user_id, note, created FROM up_orders ORDER BY id"
    snap_take dec_rows 1 "SELECT id, amount, note FROM up_dec ORDER BY id"
    snap_take item_rows 1 "SELECT a, b, v FROM up_item ORDER BY a, b"
    snap_take mpk_rows 2 "SELECT k1, k2, v FROM up_mpk ORDER BY k1, k2"
    snap_take ai_ids 0 "SELECT id FROM up_ai ORDER BY id"
    local scans; scans="$(up_floor_scans)"

    echo "== kill -9 the whole new cluster, restart on the same dirs =="
    up_stop_cluster KILL
    up_assert_all_dead "kill -9: all 3 nodes died together"
    RDB_BIN="$NEW_BIN"
    LEADER="$(up_start_all)" || return 1
    assert_eventually "restart: mysql wire answers SELECT 1" poll_has 0 "SELECT 1" "1" 60
    assert_eq "durable floor key path: no second boot floor scan" "$scans" "$(up_floor_scans)"
    snap_eq tables "post-restart: SHOW TABLES intact" 2 "SHOW TABLES" sorted
    snap_eq orders_rows "post-restart: old-table rows intact" 0 \
        "SELECT id, user_id, note, created FROM up_orders ORDER BY id"
    snap_eq dec_rows "post-restart: decimal rows intact" 2 "SELECT id, amount, note FROM up_dec ORDER BY id"
    snap_eq item_rows "post-restart: composite-pk rows intact" 0 "SELECT a, b, v FROM up_item ORDER BY a, b"
    snap_eq mpk_rows "post-restart: StarRocks multi-col pk rows intact" 1 \
        "SELECT k1, k2, v FROM up_mpk ORDER BY k1, k2"
    snap_eq ai_ids "post-restart: AUTO_INCREMENT rows intact" 1 "SELECT id FROM up_ai ORDER BY id"
    sql_mysql "$LEADER" "INSERT INTO up_ai (note) VALUES ('post-restart')"
    assert_ok "auto insert after the crash restart (leader node$LEADER)"
    sql_mysql 0 "SELECT MAX(id), COUNT(*) FROM up_ai"
    assert_eq "counter continued at 193 after the restart (65 -> 129 -> 193)" "193,16" "$(norm_rows)"
}

main () {
    e2e_init "$SCENARIO"
    trap up_cleanup EXIT
    SQL_ERR_FILE="$E2E_WORKDIR/mysql_err.txt"; : >"$SQL_ERR_FILE"
    phase0_build || exit 1
    phaseA_seed || exit 1
    phaseB_verify || exit 1
    phaseC_restart || exit 1
    e2e_finish "$SCENARIO"
}
main "$@"
