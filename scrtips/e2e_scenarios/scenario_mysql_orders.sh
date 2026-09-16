#!/usr/bin/env bash
# scenario_mysql_orders.sh - e2e: "orders / funds" story driven by REAL SQL
# CLIs (/usr/bin/mysql, /usr/local/bin/mycli) against a 3-node rdb cluster
# assembled by env.sh. Steps: (a) DDL on the raft leader + catalog spread,
# (b) DDL failure modes, (c) cross-node transactional transfer, (d) unique
# enforcement (staged veto at COMMIT + autocommit veto), (e) EXPLAIN plan
# shape, (f) node restart durability, (g) dual-CLI consistency.
#
# Semantics verified against source/tests BEFORE writing this script:
# - env.sh does NOT run CLUSTER INIT; SQL routing materializes only after
#   "CLUSTER INIT <resp0>,<resp1>,<resp2>" on the raft leader (reply
#   "done", src/command/cluster.rs:292-325), then reaches every node via
#   the 3s topology ticker (src/main.rs:79-111). Without it routing() is
#   None (src/sql/dist/mod.rs:97) and writes stay node-local, so this
#   script runs CLUSTER INIT itself and polls EXPLAIN until plans carry a
#   "Gather(bands=" headline (src/sql/dist/gather.rs:80-98).
# - Table constraints other than PRIMARY KEY are rejected with
#   "not supported in this version: other table constraints"
#   (src/sql/parse/translate.rs:527, src/sql/parse/error.rs:62-65); an
#   inline column UNIQUE is silently ignored (translate.rs:597). The
#   supported uniqueness form is CREATE UNIQUE INDEX on ONE column
#   (translate.rs:307-323), enforced incl. the distributed 2PC veto
#   (src/sql/index/mod.rs:336-345, src/sql/dist/participant.rs:147-163).
# - Duplicate table: "table 'x' already exists" errno 1050
#   (src/sql/exec/ddl.rs:182-185); duplicate index: "index 'x' already
#   exists" (ddl.rs:289-292). DDL is leader-gated: non-leaders answer
#   "<what> requires the raft leader" (src/sql/storage/catalog.rs:108-119),
#   hence ddl_exec() retries while the stderr mentions "leader".
# - Unique conflicts inside a txn are rejected at STATEMENT time with
#   native errno 1062 (real InnoDB semantics), so the scenario rolls
#   back after the rejected INSERT and proves nothing leaked. The
#   message differs by path (local "Duplicate entry ..." vs 2PC
#   "dup: unique value already owned by another row"); both carry
#   errno 1062 -> dup assertions require 1062 and accept either
#   message (assert_dup_1062). DDL inside a txn:
#   "DDL not allowed inside a transaction" (exec/mod.rs:108-122).
# - EXPLAIN renders one "plan" column, one line per node
#   (src/sql/exec/render.rs:148-159,213,223); in routed cluster mode the
#   plan is ALWAYS Gather(bands=N) + SeqScan <table> + Filter: <expr> --
#   IndexScan never appears (src/sql/exec/select.rs:98-134,
#   tests/sql_dist_read_e2e.rs:265-292).
# - SHOW TABLES/COLUMNS/INDEX (src/sql/exec/show.rs:60-124): Key flag PRI
#   for the pk column, UNI for the unique-index column; SHOW INDEX rows
#   are "<table>,<non_unique>,<key_name>,1,<column>,BTREE".
# - Auth: the server offers only mysql_native_password with the empty
#   config password, so both CLIs connect with "-u root" and NO -p
#   (src/sql/front/shim.rs:29-30,109-119, src/sql/front/auth.rs:40-46).
# - CLI shapes (verified empirically against a dead port): mysql
#   --no-defaults -B -N --raw -e "s1; s2" sends both over ONE connection
#   and stops at the first failure; rows are tab-separated WITHOUT header;
#   errors go to stderr as "ERROR <errno> (<sqlstate>): <msg>", rc=1.
#   mycli --csv -e splits ";" the same way; batch CSV ALWAYS starts with
#   the header row (cli_helpers delimited_output_adapter), so sql_mycli
#   strips it; errors go to stderr as "(<errno>, \"<msg>\")", rc=1.
#   Arithmetic UPDATE "SET amount = amount - 40": precedent
#   tests/sql_txn_e2e.rs:153 ("SET score = score + 1").

# env.sh installs `set -uo pipefail` and the assertion helpers. Deliberately
# NO `set -e`: assertion failures count into E2E_FAILS so the scenario
# always runs to its e2e_finish summary.
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

# RESP calls (raft leader discovery + CLUSTER INIT) require the raft token.
export REDISCLI_AUTH="${RDB_E2E_TOKEN}"
RC="redis-cli -h $E2E_HOST -p"

rc_resp () { # one RESP command against node <idx>: idx args...
    local idx=$1
    shift
    $RC "$(node_resp "$idx")" --raw "$@" 2>/dev/null
}

# Index of the node whose "raft nodes" reports "[Leader]" (src/command/
# raft_cmd.rs:44-49,149); empty/no leader -> rc 1.
find_leader_idx () {
    local i out
    for i in 0 1 2; do
        out="$(rc_resp "$i" raft nodes)"
        case "$out" in
        *"[Leader]"*) echo "$i"; return 0 ;;
        esac
    done
    return 1
}

# ---- SQL client wrappers: set SQL_OUT / SQL_ERR_TEXT / SQL_RC -----------
MYSQL_CLI="$(command -v mysql  || true)"
MYCLI_CLI="$(command -v mycli || true)"
SQL_ERR_FILE=""   # stderr scratch file, set after e2e_init

sql_mysql () { # <idx> <sql>: headerless tab-separated rows on stdout
    local port
    port="$(node_mysql "$1")"
    SQL_OUT="$(mysql --no-defaults -h "$E2E_HOST" -P "$port" -u root \
        --connect-timeout=5 -B -N --raw -e "$2" 2>"$SQL_ERR_FILE")"
    SQL_RC=$?
    SQL_ERR_TEXT="$(cat "$SQL_ERR_FILE")"
}

sql_mycli () { # <idx> <sql>: csv WITH header -> strip header + quotes here
    local port raw
    port="$(node_mysql "$1")"
    raw="$(mycli --myclirc /dev/null --defaults-file /dev/null \
        -h "$E2E_HOST" -P "$port" -u root --csv -e "$2" 2>"$SQL_ERR_FILE")"
    SQL_RC=$?
    SQL_ERR_TEXT="$(cat "$SQL_ERR_FILE")"
    # normalize to the mysql shape: no header, no quotes, comma-separated
    SQL_OUT="$(printf '%s\n' "$raw" | tail -n +2 | tr -d '"\r' | tr '\t' ',')"
}

# ---- local assertions (extend env.sh's; same E2E_FAILS accounting) ------
assert_ok () { # desc: last sql_* call must succeed
    if [ "$SQL_RC" -eq 0 ]; then
        echo "ok   - $1"
    else
        E2E_FAILS=$((E2E_FAILS + 1))
        echo "FAIL: $1: rc=$SQL_RC stderr=[$SQL_ERR_TEXT]" >&2
    fi
}

assert_fails () { # desc: last sql_* call must fail
    if [ "$SQL_RC" -ne 0 ]; then
        echo "ok   - $1"
    else
        E2E_FAILS=$((E2E_FAILS + 1))
        echo "FAIL: $1: expected a failure but rc=0 out=[$SQL_OUT]" >&2
    fi
}

assert_err_contains () { # desc needle: last call must have failed WITH needle
    if [ "$SQL_RC" -eq 0 ]; then
        E2E_FAILS=$((E2E_FAILS + 1))
        echo "FAIL: $1: expected failure, but rc=0" >&2
    elif [[ "$SQL_ERR_TEXT" == *"$2"* ]]; then
        echo "ok   - $1"
    else
        E2E_FAILS=$((E2E_FAILS + 1))
        echo "FAIL: $1: [$2] not in stderr [$SQL_ERR_TEXT]" >&2
    fi
}

# Unique-violation text differs by commit path (local backfill check vs
# distributed 2PC veto); both carry native errno 1062 -> accept either.
assert_dup_1062 () { # desc (checks the LAST sql_* call)
    assert_fails "$1 (statement rejected)"
    assert_err_contains "$1 (native errno 1062)" "1062"
    case "$SQL_ERR_TEXT" in
    *"Duplicate entry"*|*"dup: unique value"*)
        echo "ok   - $1 (dup message present)"
        ;;
    *)
        E2E_FAILS=$((E2E_FAILS + 1))
        echo "FAIL: $1: no dup message in [$SQL_ERR_TEXT]" >&2
        ;;
    esac
}

# DDL with leader retry: non-leaders answer "... requires the raft leader"
# (catalog.rs:108-119); poll a few times before surfacing the last result.
ddl_exec () { # <idx> <sql> -> SQL_OUT/SQL_ERR_TEXT/SQL_RC of the last try
    local idx=$1 sql=$2 tries=0
    while :; do
        sql_mysql "$idx" "$sql"
        if [ "$SQL_RC" -eq 0 ] || [[ "${SQL_ERR_TEXT#*leader}" == "$SQL_ERR_TEXT" ]]; then
            return 0
        fi
        tries=$((tries + 1))
        [ "$tries" -ge 10 ] && return 0
        sleep 1
    done
}

# ---- polls: each leaves POLL_OUT holding the last statement's output ----
poll_has () { # idx sql needle timeout -> 0 when output contains needle
    local deadline=$((SECONDS + $4))
    POLL_OUT=""
    while :; do
        sql_mysql "$1" "$2"
        if [ "$SQL_RC" -eq 0 ] && [[ "$SQL_OUT" == *"$3"* ]]; then
            POLL_OUT="$SQL_OUT"
            return 0
        fi
        [ "$SECONDS" -ge "$deadline" ] && return 1
        sleep 1
    done
}

wait_mysql () { # idx [timeout] -> 0 when "SELECT 1" answers 1
    local deadline=$((SECONDS + ${2:-60}))
    while [ "$SECONDS" -lt "$deadline" ]; do
        sql_mysql "$1" "SELECT 1"
        { [ "$SQL_RC" -eq 0 ] && [ "$SQL_OUT" = "1" ]; } && return 0
        sleep 1
    done
    return 1
}

wait_routing () { # idx timeout -> 0 when EXPLAIN carries a Gather headline
    local deadline=$((SECONDS + $2))
    while [ "$SECONDS" -lt "$deadline" ]; do
        sql_mysql "$1" "EXPLAIN SELECT id FROM orders_t WHERE id = 1"
        { [ "$SQL_RC" -eq 0 ] && [[ "$SQL_OUT" == *"Gather(bands="* ]]; } && return 0
        sleep 1
    done
    return 1
}

# One assertion either way: ok when cmd succeeds, timeout when it does not.
assert_eventually () { # desc cmd args...
    local desc=$1
    shift
    if "$@"; then
        assert_eq "$desc" "ok" "ok"
    else
        assert_eq "$desc" "ok" "timeout"
    fi
}

# Rows of "SELECT id, amount ... ORDER BY id" as one line: "1001,60;1002,90"
balances_via () { # idx -> prints normalized balances (ERR:<rc> on failure)
    sql_mysql "$1" "SELECT id, amount FROM orders_t ORDER BY id"
    if [ "$SQL_RC" -eq 0 ]; then
        printf '%s' "$SQL_OUT" | tr '\t' ',' | paste -sd';' -
    else
        echo "ERR:$SQL_RC"
    fi
}

main () {
    local scenario="mysql_orders"
    e2e_init "$scenario"
    SQL_ERR_FILE="$E2E_WORKDIR/sql_cli_err.txt"

    if [ -z "$MYSQL_CLI" ] || [ -z "$MYCLI_CLI" ]; then
        echo "FATAL: mysql and/or mycli client not found in PATH" >&2
        E2E_FAILS=$((E2E_FAILS + 1))
        e2e_finish "$scenario"
    fi

    e2e_start_cluster || {
        echo "FATAL: cluster bring-up failed; aborting scenario" >&2
        exit 1
    }

    # ---- cluster metadata: raft leader + CLUSTER INIT + SQL readiness ----
    local leader init_reply mysql_rows
    leader="$(find_leader_idx)" || {
        echo "FATAL: no raft leader reported by 'raft nodes'" >&2
        exit 1
    }
    init_reply="$(rc_resp "$leader" cluster init \
        "$E2E_HOST:$(node_resp 0),$E2E_HOST:$(node_resp 1),$E2E_HOST:$(node_resp 2)")"
    assert_eq "cluster init on leader (node$leader) replies done" "done" "$init_reply"
    assert_eventually "mysql wire on node0 answers SELECT 1" wait_mysql 0 60

    # ---- (a) DDL on the leader, catalog visible on followers ------------
    ddl_exec 0 "CREATE TABLE orders_t (id BIGINT PRIMARY KEY, user_id BIGINT, amount BIGINT, note VARCHAR(64))"
    assert_ok "CREATE TABLE orders_t on leader"
    ddl_exec 0 "CREATE UNIQUE INDEX uk_user ON orders_t (user_id)"
    assert_ok "CREATE UNIQUE INDEX uk_user on leader"
    assert_eventually "catalog replicated: node1 SHOW TABLES lists orders_t" \
        poll_has 1 "SHOW TABLES" "orders_t" 30

    sql_mysql 1 "SHOW TABLES"
    assert_ok "SHOW TABLES on follower node1"
    assert_contains "SHOW TABLES output names orders_t" "orders_t" "$SQL_OUT"

    sql_mysql 1 "SHOW COLUMNS FROM orders_t"
    assert_ok "SHOW COLUMNS on follower node1"
    assert_contains "id column carries the PRI key flag" "PRI" "$SQL_OUT"
    assert_contains "user_id row present for the unique index" "user_id" "$SQL_OUT"

    sql_mysql 2 "SHOW INDEX FROM orders_t"
    assert_ok "SHOW INDEX on node2"
    # mysql batch output is TAB-separated; normalize to commas first.
    index_csv="$(printf '%s' "$SQL_OUT" | tr '\t' ',')"
    assert_contains "uk_user index row (unique, on user_id)" \
        "orders_t,0,uk_user,1,user_id,BTREE" "$index_csv"
    assert_contains "implicit PRIMARY index row on id" \
        "orders_t,0,PRIMARY,1,id,BTREE" "$index_csv"

    # ---- (b) DDL failure modes ------------------------------------------
    ddl_exec 0 "CREATE TABLE orders_bad_t (id BIGINT PRIMARY KEY, user_id BIGINT, UNIQUE KEY uk_bad (user_id))"
    assert_fails "table-constraint UNIQUE KEY rejected"
    assert_err_contains "rejection is the v1 grammar error" "not supported in this version"
    sql_mysql 2 "SHOW TABLES"
    assert_not_contains "rejected DDL left no orders_bad_t behind" "orders_bad_t" "$SQL_OUT"

    ddl_exec 0 "CREATE TABLE orders_t (id BIGINT PRIMARY KEY)"
    assert_err_contains "duplicate CREATE TABLE rejected" "table 'orders_t' already exists"
    assert_err_contains "duplicate table carries errno 1050" "1050"
    ddl_exec 0 "CREATE UNIQUE INDEX uk_user ON orders_t (user_id)"
    assert_err_contains "duplicate CREATE INDEX rejected" "index 'uk_user' already exists"

    sql_mysql 0 "BEGIN; CREATE INDEX idx_note ON orders_t (note)"
    assert_err_contains "DDL inside a transaction rejected" "DDL not allowed inside a transaction"

    # ---- routing: the topology ticker must have published the sql nodes --
    assert_eventually "node0 EXPLAIN shows a routed Gather plan" wait_routing 0 30
    wait_routing 1 30 && wait_routing 2 30

    # ---- (c) seed + cross-node transactional transfer --------------------
    sql_mysql 0 "INSERT INTO orders_t (id, user_id, amount, note) VALUES (1001, 101, 100, 'alice')"
    assert_ok "seed insert alice via node0"
    sql_mysql 0 "INSERT INTO orders_t (id, user_id, amount, note) VALUES (1002, 102, 50, 'bob')"
    assert_ok "seed insert bob via node0"

    poll_has 2 "SELECT COUNT(*) FROM orders_t" "2" 30
    assert_eq "rows visible cross-node from node2 (count=2)" "2" "$POLL_OUT"

    sql_mysql 1 "BEGIN"
    assert_ok "BEGIN on node1 (coordinator != leader)"
    sql_mysql 1 "UPDATE orders_t SET amount = amount - 40 WHERE id = 1001"
    assert_ok "debit 40 from alice (staged)"
    sql_mysql 1 "UPDATE orders_t SET amount = amount + 40 WHERE id = 1002"
    assert_ok "credit 40 to bob (staged)"
    sql_mysql 1 "COMMIT"
    assert_ok "COMMIT of the cross-node transfer"

    assert_eq "balances after transfer (read via node2)" "1001,60;1002,90" "$(balances_via 2)"
    assert_eq "same rows via node0 (gather equality)" "1001,60;1002,90" "$(balances_via 0)"

    # ---- (d) unique enforcement: staged veto, rollback, autocommit veto --
    sql_mysql 1 "BEGIN"
    assert_ok "BEGIN for the duplicate-funds attempt"
    sql_mysql 1 "INSERT INTO orders_t (id, user_id, amount, note) VALUES (7, 101, 10, 'dup-u101')"
    assert_dup_1062 "duplicate INSERT inside the txn rejected at stage time"

    sql_mysql 1 "ROLLBACK"
    assert_ok "ROLLBACK after the rejected INSERT still succeeds"
    assert_eq "balances unchanged after the rejected insert" \
        "1001,60;1002,90" "$(balances_via 2)"
    sql_mysql 0 "SELECT COUNT(*) FROM orders_t"
    assert_eq "no row leaked from the failed txn" "2" "$SQL_OUT"

    sql_mysql 2 "INSERT INTO orders_t (id, user_id, amount, note) VALUES (8, 102, 5, 'dup-u102')"
    assert_dup_1062 "autocommit duplicate INSERT rejected on node2"
    sql_mysql 1 "SELECT COUNT(*) FROM orders_t"
    assert_eq "count still 2 after the autocommit veto" "2" "$SQL_OUT"

    # ---- (e) EXPLAIN: always the distributed Gather plan -----------------
    sql_mysql 1 "EXPLAIN SELECT id FROM orders_t WHERE id = 1001"
    assert_ok "EXPLAIN on node1"
    assert_contains "plan headline is Gather(bands=N)" "Gather(bands=" "$SQL_OUT"
    assert_contains "plan scans orders_t" "SeqScan orders_t" "$SQL_OUT"
    assert_contains "plan carries the filter" "Filter: id = 1001" "$SQL_OUT"
    assert_not_contains "v1 never plans an IndexScan" "IndexScan" "$SQL_OUT"

    # ---- (f) restart durability -----------------------------------------
    assert_eventually "node1 restarts (state reused after kill)" e2e_restart_node 1
    assert_eventually "mysql wire back on restarted node1" wait_mysql 1 60
    assert_eq "committed balances survive the node1 restart" \
        "1001,60;1002,90" "$(balances_via 1)"

    sql_mysql 1 "INSERT INTO orders_t (id, user_id, amount, note) VALUES (1003, 103, 7, 'carol')"
    assert_ok "restarted node1 accepts writes again"
    poll_has 2 "SELECT COUNT(*) FROM orders_t" "3" 30
    assert_eq "node2 sees carol after the node1 write" "3" "$POLL_OUT"

    assert_eventually "node2 restarts" e2e_restart_node 2
    assert_eventually "mysql wire back on restarted node2" wait_mysql 2 60
    assert_eq "all three rows survive the node2 restart" \
        "1001,60;1002,90;1003,7" "$(balances_via 2)"

    # ---- (g) dual-CLI consistency ----------------------------------------
    sql_mysql 0 "SELECT id, amount FROM orders_t ORDER BY id"
    mysql_rows="$SQL_OUT"
    sql_mycli 0 "SELECT id, amount FROM orders_t ORDER BY id"
    assert_ok "mycli query on node0 succeeds"
    assert_eq "mysql and mycli rows are identical on node0" \
        "$(printf '%s' "$mysql_rows" | tr '\t' ',' | paste -sd';' -)" \
        "$(printf '%s' "$SQL_OUT" | paste -sd';' -)"

    sql_mycli 1 "SELECT id, amount FROM orders_t ORDER BY id"
    assert_ok "mycli query on node1 succeeds"
    assert_eq "mycli on node1 matches mysql on node2" "$(balances_via 2)" \
        "$(printf '%s' "$SQL_OUT" | paste -sd';' -)"

    sql_mycli 1 "BEGIN"
    assert_ok "mycli BEGIN"
    sql_mycli 1 "INSERT INTO orders_t (id, user_id, amount, note) VALUES (9, 101, 1, 'mycli-dup')"
    assert_dup_1062 "mycli duplicate INSERT rejected at stage time"
    sql_mycli 1 "ROLLBACK"
    assert_ok "mycli ROLLBACK after the rejected INSERT"

    assert_eq "balances final (via mysql on node0)" \
        "1001,60;1002,90;1003,7" "$(balances_via 0)"

    # ---- (h) DECIMAL(p,s) amounts (W2.0): exact math, rounding, SUM/AVG,
    # secondary-index point lookup, DESCRIBE type, precision edge.
    # Semantics from tests/sql_types_e2e.rs decimal_exact_semantics:
    # 0.1+0.2 == 0.3 exactly; writes round half-away-from-zero at the
    # declared scale; SUM keeps the column scale, AVG divides at
    # scale+4; DECIMAL(5,2) rejects 1000 with MySQL 1292. EXPLAIN in
    # routed cluster mode is ALWAYS Gather(bands=N)+SeqScan (indexes
    # only cover the owning band, src/sql/exec/select.rs:113-121), so
    # the point lookup is asserted on its RESULT rows, plus the Gather
    # headline documenting the routed plan shape.
    ddl_exec 0 "CREATE TABLE orders_amt (id BIGINT PRIMARY KEY, amount DECIMAL(10,2), note VARCHAR(64))"
    assert_ok "CREATE TABLE orders_amt on leader"
    ddl_exec 0 "CREATE INDEX idx_amount ON orders_amt (amount)"
    assert_ok "CREATE INDEX idx_amount on leader"
    assert_eventually "catalog replicated: node1 SHOW TABLES lists orders_amt" \
        poll_has 1 "SHOW TABLES" "orders_amt" 30

    sql_mysql 1 "SELECT 0.1 + 0.2"
    assert_ok "SELECT 0.1 + 0.2 evaluates on node1"
    assert_eq "decimal literals add exactly (0.1+0.2 == 0.3, no float tail)" \
        "0.3" "$SQL_OUT"

    sql_mysql 0 "INSERT INTO orders_amt (id, amount, note) VALUES (1, 0.1, 'a'), (2, 0.2, 'b'), (3, 1.005, 'c'), (4, -1.005, 'd')"
    assert_ok "seed orders_amt (0.1, 0.2, 1.005, -1.005)"
    poll_has 2 "SELECT COUNT(*) FROM orders_amt" "4" 30
    assert_eq "all four decimal rows visible cross-node from node2" "4" "$POLL_OUT"

    sql_mysql 2 "SELECT amount FROM orders_amt ORDER BY id"
    assert_ok "read back amounts via node2"
    assert_eq "DECIMAL(10,2) renders rounded half-away-from-zero" \
        "0.10;0.20;1.01;-1.01" "$(printf '%s' "$SQL_OUT" | paste -sd';' -)"

    sql_mysql 1 "SELECT SUM(amount) FROM orders_amt"
    assert_ok "SUM over the decimal column"
    assert_eq "SUM stays at the column scale (0.10+0.20+1.01-1.01)" "0.30" "$SQL_OUT"
    sql_mysql 1 "SELECT AVG(amount) FROM orders_amt"
    assert_ok "AVG over the decimal column"
    assert_eq "AVG widens to scale+4 (0.30/4 at scale 6)" "0.075000" "$SQL_OUT"

    sql_mysql 0 "SELECT id, note FROM orders_amt WHERE amount = 0.10"
    assert_ok "point lookup on the decimal secondary index"
    assert_eq "WHERE amount = 0.10 hits exactly the id=1 row" \
        "1,a" "$(printf '%s' "$SQL_OUT" | tr '\t' ',')"
    sql_mysql 1 "EXPLAIN SELECT id FROM orders_amt WHERE amount = 0.10"
    assert_ok "EXPLAIN the decimal point lookup"
    assert_contains "routed plan stays Gather (cluster indexes are band-local)" \
        "Gather(bands=" "$SQL_OUT"

    sql_mysql 2 "SHOW COLUMNS FROM orders_amt"
    assert_ok "SHOW COLUMNS orders_amt"
    assert_contains "amount column type renders decimal(10,2)" "decimal(10,2)" "$SQL_OUT"
    assert_contains "amount carries the MUL flag (non-unique index)" "MUL" "$SQL_OUT"
    sql_mysql 2 "SHOW INDEX FROM orders_amt"
    assert_ok "SHOW INDEX orders_amt"
    assert_contains "idx_amount index row (non-unique, on amount)" \
        "orders_amt,1,idx_amount,1,amount,BTREE" \
        "$(printf '%s' "$SQL_OUT" | tr '\t' ',')"

    ddl_exec 0 "CREATE TABLE orders_p5 (id BIGINT PRIMARY KEY, amount DECIMAL(5,2))"
    assert_ok "CREATE TABLE orders_p5 (DECIMAL(5,2) precision edge)"
    sql_mysql 0 "INSERT INTO orders_p5 (id, amount) VALUES (1, 999.99)"
    assert_ok "DECIMAL(5,2) accepts the 999.99 edge"
    sql_mysql 0 "INSERT INTO orders_p5 (id, amount) VALUES (2, 1000)"
    assert_err_contains "DECIMAL(5,2) rejects 1000 (MySQL errno 1292)" "1292"
    assert_err_contains "DECIMAL(5,2) rejection names the range" "Out of range"
    sql_mysql 0 "SELECT COUNT(*) FROM orders_p5"
    assert_eq "rejected out-of-range insert left no row" "1" "$SQL_OUT"

    # ---- (i) composite PRIMARY KEY(order_id, seq) (W2.1) ---------------
    # Row-store INSERT is INSERT-as-UPSERT on the pk (tx::stage_upsert
    # writes the pk key without a dup check), so a repeated pk
    # REPLACES its row -- single- and multi-column pks behave alike.
    # First observe the single-column behavior on orders_amt, then
    # assert the composite pk matches it. Composite pk columns are
    # narrowed to Int/VarChar/Date/DateTime (src/sql/exec/ddl.rs:578-
    # 598) and AUTO_INCREMENT in a composite pk is MySQL 1075
    # (ER_WRONG_AUTO_KEY, ddl.rs:611-644).
    ddl_exec 0 "CREATE TABLE orders_item (order_id BIGINT, seq INT, sku VARCHAR(32), qty INT, PRIMARY KEY(order_id, seq))"
    assert_ok "CREATE TABLE orders_item (composite pk) on leader"
    assert_eventually "catalog replicated: node1 SHOW TABLES lists orders_item" \
        poll_has 1 "SHOW TABLES" "orders_item" 30

    sql_mysql 0 "INSERT INTO orders_amt (id, amount, note) VALUES (1, 9.99, 'a-again')"
    assert_ok "single-column pk: re-INSERT of id=1 succeeds (upsert baseline)"
    poll_has 1 "SELECT amount FROM orders_amt WHERE id = 1" "9.99" 30
    assert_eq "single-column re-INSERT replaced the row in place" "9.99" "$POLL_OUT"
    sql_mysql 0 "SELECT COUNT(*) FROM orders_amt"
    assert_eq "row count unchanged after the single-column re-INSERT" "4" "$SQL_OUT"

    sql_mysql 0 "INSERT INTO orders_item (order_id, seq, sku, qty) VALUES (500, 1, 'sku-a', 2), (500, 2, 'sku-b', 3)"
    assert_ok "seed two line items of order 500"
    sql_mysql 0 "INSERT INTO orders_item (order_id, seq, sku, qty) VALUES (500, 1, 'sku-a2', 5)"
    assert_ok "composite pk: re-INSERT of (500,1) succeeds like the single-column case"
    poll_has 2 "SELECT sku FROM orders_item WHERE order_id = 500 AND seq = 1" "sku-a2" 30
    assert_eq "re-INSERT (500,1) carries the NEW row" "sku-a2" "$POLL_OUT"
    sql_mysql 2 "SELECT COUNT(*) FROM orders_item"
    assert_eq "re-INSERT (500,1) replaced, not appended" "2" "$SQL_OUT"

    sql_mysql 1 "INSERT INTO orders_item (order_id, seq, sku, qty) VALUES (500, 3, 'sku-c', 7)"
    assert_ok "a third seq under the SAME order_id coexists"
    poll_has 0 "SELECT COUNT(*) FROM orders_item" "3" 30
    assert_eq "same order_id, different seq rows all present" "3" "$POLL_OUT"
    sql_mysql 1 "SELECT order_id, seq, sku, qty FROM orders_item ORDER BY seq"
    assert_ok "read back the line items via node1"
    assert_eq "line items keyed on the full (order_id, seq) tuple" \
        "500,1,sku-a2,5;500,2,sku-b,3;500,3,sku-c,7" \
        "$(printf '%s' "$SQL_OUT" | tr '\t' ',' | paste -sd';' -)"

    sql_mysql 2 "SELECT sku FROM orders_item WHERE order_id = 500 AND seq = 2"
    assert_ok "full-tuple point lookup via node2"
    assert_eq "WHERE order_id=? AND seq=? fetches one row" "sku-b" "$SQL_OUT"
    sql_mysql 2 "UPDATE orders_item SET sku = 'sku-b2', qty = 4 WHERE order_id = 500 AND seq = 2"
    assert_ok "UPDATE by the full composite key"
    poll_has 1 "SELECT sku FROM orders_item WHERE order_id = 500 AND seq = 2" "sku-b2" 30
    assert_eq "UPDATE rewrote the whole-key-matched row" "sku-b2" "$POLL_OUT"

    sql_mysql 1 "SHOW COLUMNS FROM orders_item"
    assert_ok "SHOW COLUMNS orders_item"
    item_cols="$(printf '%s' "$SQL_OUT" | tr '\t' ',')"
    assert_contains "order_id row carries the PRI flag" "order_id,bigint,NO,PRI" "$item_cols"
    assert_contains "seq row carries the PRI flag too" "seq,bigint,NO,PRI" "$item_cols"
    sql_mysql 1 "SHOW INDEX FROM orders_item"
    assert_ok "SHOW INDEX orders_item"
    item_index_csv="$(printf '%s' "$SQL_OUT" | tr '\t' ',')"
    assert_contains "PRIMARY index row seq 1 on order_id" \
        "orders_item,0,PRIMARY,1,order_id,BTREE" "$item_index_csv"
    assert_contains "PRIMARY index row seq 2 on seq" \
        "orders_item,0,PRIMARY,2,seq,BTREE" "$item_index_csv"

    sql_mysql 0 "CREATE TABLE orders_ai (order_id BIGINT, seq INT AUTO_INCREMENT, sku VARCHAR(32), PRIMARY KEY(order_id, seq))"
    assert_err_contains "composite pk containing AUTO_INCREMENT rejected (errno 1075)" "1075"
    assert_err_contains "1075 rejection is the auto-key message" "auto column"
    sql_mysql 0 "SHOW TABLES"
    assert_not_contains "rejected orders_ai was NOT created" "orders_ai" "$SQL_OUT"

    e2e_finish "$scenario"
}

main "$@"
