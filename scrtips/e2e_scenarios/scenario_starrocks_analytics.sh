#!/usr/bin/env bash
# scenario_starrocks_analytics.sh -- StarRocks table-model DDL compat face,
# driven through REAL CLIs (mycli + the official mysql client) against a real
# 3-node rdb cluster spawned by env.sh.
#
# Semantics asserted here are taken from the source, not guessed:
#   * src/sql/parse/starrocks.rs       -- clause lifting + loud rejections
#   * src/sql/exec/ddl.rs:457-505      -- model/engine matrix, BUCKETS check
#   * src/sql/exec/write.rs:143+       -- columnar INSERT limits, append-only
#   * tests/starrocks_model_e2e.rs     -- the executable specification
#
# Models:
#   PRIMARY KEY(col) [DISTRIBUTED BY HASH(col) [BUCKETS n]] -> row store,
#     INSERT-as-UPSERT (re-inserting a pk REPLACES its row).
#   DUPLICATE KEY(cols) [DISTRIBUTED BY HASH(col) [BUCKETS n]] -> columnar,
#     append-only (same key twice stays twice).
# Rejections are MySQL 1235 (ER_NOT_SUPPORTED_YET) with a clause-naming
# message ("StarRocks ..."); BUCKETS 0 is 1064, unknown dist column 1054.
#
# All SQL runs against the raft-LEADER node's mysql port: catalog DDL fails
# on followers ("... requires the raft leader", src/sql/storage/catalog.rs),
# and leader-side reads avoid read-point lag between members (COMPAT.md
# "Timestamps"); value assertions still poll so 2PC/gather lag cannot flake.
set -u
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

SCENARIO=starrocks_analytics
# mycli 1.31.2 has NO --no-rc; neutralize rc/cnf reads explicitly instead
# (--defaults-file replaces cnf_files, --myclirc /dev/null skips user rc).
# The server (env.sh) configures root + empty password, so no password flag
# is needed and none may be hardcoded.
MYCLI=(mycli --defaults-file /dev/null --myclirc /dev/null -h "$E2E_HOST" -u root)
# -N skips headers, -B is batch TSV; --no-defaults ignores /etc/my.cnf etc.
MYSQL=(mysql --no-defaults --protocol=TCP -h "$E2E_HOST" -u root -N -B --connect-timeout=3)

# ---- SQL wrappers -------------------------------------------------------
# sql_exec <port> <sql>: mycli, TSV (header + rows) on success, client error
# text on stderr; stdout+stderr merged so callers can assert error text.
sql_exec () {
    "${MYCLI[@]}" -P "$1" -e "$2" 2>&1
}

# sql_raw <port> <sql>: the official mysql client, headerless TSV.
sql_raw () {
    "${MYSQL[@]}" -P "$1" -e "$2" 2>&1
}

# value rows of a result: mycli prints a header first, so strip it. Works
# for single values (one row) AND multi-row bodies (GROUP BY ledger); on a
# client error the merged output has no header, so this yields "" and the
# surrounding assert fails with the mismatch.
sql_scalar () {
    sql_exec "$1" "$2" | tail -n +2
}

# wait_mysql <port>: poll until the mysql wire answers SELECT 1 (<= 60s).
wait_mysql () {
    local port=$1 i
    for i in $(seq 1 60); do
        [ "$(sql_raw "$port" 'SELECT 1')" = "1" ] && return 0
        sleep 1
    done
    return 1
}

# leader_idx: echo the node idx whose /metrics reports raft leader (<= 30s).
# (No `curl | grep -q`: under pipefail an early grep exit can SIGPIPE curl
# and flip the status; match against the captured body instead.)
leader_idx () {
    local idx tick metrics
    for tick in $(seq 1 30); do
        for idx in 0 1 2; do
            metrics=$(curl -sf "http://$E2E_HOST:$(node_monitor "$idx")/metrics" 2>/dev/null) || continue
            case "$metrics" in
            *'status="Leader"'*) echo "$idx"; return 0 ;;
            esac
        done
        sleep 1
    done
    return 1
}

# expect_ok <desc> <port> <sql>: statement must succeed.
expect_ok () {
    local out rc
    out=$(sql_exec "$2" "$3")
    rc=$?
    assert_eq "$1 (exit status)" "0" "$rc"
    if [ "$rc" -ne 0 ]; then
        echo "     sql: $3" >&2
        echo "     got: $out" >&2
    fi
}

# expect_err <desc> <port> <sql> <needle>...: statement must fail (client
# exit 1) and its merged output must contain EVERY needle.
expect_err () {
    local desc=$1 port=$2 sql=$3 out rc needle
    shift 3
    out=$(sql_exec "$port" "$sql")
    rc=$?
    assert_eq "$desc (rejected, exit status)" "1" "$rc"
    for needle in "$@"; do
        assert_contains "$desc" "$needle" "$out"
    done
}

# wait_value <desc> <port> <sql> <want>: poll a scalar until it equals
# <want> (bounded); assert regardless at the end.
wait_value () {
    local desc=$1 port=$2 sql=$3 want=$4 got="" i
    for i in $(seq 1 60); do
        got=$(sql_scalar "$port" "$sql")
        [ "$got" = "$want" ] && break
        sleep 0.5
    done
    assert_eq "$desc" "$want" "$got"
}

# ---- bring-up -----------------------------------------------------------
e2e_init "$SCENARIO"

e2e_start_cluster || {
    echo "FATAL: cluster did not come up" >&2
    e2e_finish "$SCENARIO"
}

PORT=$(node_mysql 0)
wait_mysql "$PORT" || {
    echo "FATAL: mysql wire never answered on $PORT" >&2
    e2e_finish "$SCENARIO"
}

LEADER=$(leader_idx) || {
    echo "FATAL: no raft leader visible in /metrics" >&2
    e2e_finish "$SCENARIO"
}
PORT=$(node_mysql "$LEADER")
echo "sql endpoint: leader node$LEADER mysql $PORT"

# ---- a. PK model DDL ----------------------------------------------------
# Exact accepted header shape (tests/starrocks_model_e2e.rs:145):
#   CREATE TABLE t (cols...) PRIMARY KEY(col) DISTRIBUTED BY HASH(col) BUCKETS n
expect_ok "a: CREATE sr_user PK model" "$PORT" \
    "CREATE TABLE sr_user (id BIGINT NOT NULL, name VARCHAR(64) NULL, age INT NULL) PRIMARY KEY(id) DISTRIBUTED BY HASH(id) BUCKETS 3"
assert_contains "a: sr_user in SHOW TABLES" "sr_user" \
    "$(sql_exec "$PORT" 'SHOW TABLES')"

# ---- b. INSERT-as-UPSERT ------------------------------------------------
expect_ok "b: seed sr_user (ids 1,2)" "$PORT" \
    "INSERT INTO sr_user (id, name, age) VALUES (1, 'alice', 30), (2, 'bob', 25)"
# same pk again, different values: latest wins, no duplicate error
expect_ok "b: re-INSERT id=1 (upsert)" "$PORT" \
    "INSERT INTO sr_user (id, name, age) VALUES (1, 'alice2', 31)"
wait_value "b: upsert keeps exactly 2 rows" "$PORT" \
    "SELECT COUNT(*) FROM sr_user" "2"
wait_value "b: id=1 carries the NEW name" "$PORT" \
    "SELECT name FROM sr_user WHERE id = 1" "alice2"
wait_value "b: id=1 carries the NEW age" "$PORT" \
    "SELECT age FROM sr_user WHERE id = 1" "31"
wait_value "b: only ONE visible row per pk" "$PORT" \
    "SELECT COUNT(*) FROM sr_user WHERE id = 1" "1"

# ---- c. DUPLICATE model: append-only ------------------------------------
expect_ok "c: CREATE sr_events DUPLICATE model" "$PORT" \
    "CREATE TABLE sr_events (k BIGINT NOT NULL, lvl VARCHAR(8), region VARCHAR(16)) DUPLICATE KEY(k) DISTRIBUTED BY HASH(k) BUCKETS 3"
# one statement, 10 rows, k values REPEATED: append keeps all of them
expect_ok "c: batch INSERT 10 rows (repeated k)" "$PORT" \
    "INSERT INTO sr_events (k, lvl, region) VALUES (1,'info','east'),(2,'info','west'),(3,'warn','east'),(4,'info','north'),(5,'error','west'),(1,'warn','east'),(2,'error','west'),(3,'info','north'),(4,'warn','east'),(5,'info','west')"
wait_value "c: 10 appended rows survive (no dedup)" "$PORT" \
    "SELECT COUNT(*) FROM sr_events" "10"
wait_value "c: both rows with k=1 still there" "$PORT" \
    "SELECT COUNT(*) FROM sr_events WHERE k = 1" "2"

# ---- d. aggregation: GROUP BY + COUNT/SUM over the columnar appends -----
expect_ok "d: CREATE sr_orders DUPLICATE model" "$PORT" \
    "CREATE TABLE sr_orders (k BIGINT NOT NULL, region VARCHAR(16), amount INT) DUPLICATE KEY(k) DISTRIBUTED BY HASH(k) BUCKETS 3"
expect_ok "d: seed 5 orders" "$PORT" \
    "INSERT INTO sr_orders (k, region, amount) VALUES (1,'east',10),(2,'east',20),(3,'west',5),(4,'west',15),(5,'north',7)"
wait_value "d: COUNT(*) over columnar" "$PORT" \
    "SELECT COUNT(*) FROM sr_orders" "5"
wait_value "d: SUM(amount) reconciles to 57" "$PORT" \
    "SELECT SUM(amount) FROM sr_orders" "57"
# per-region ledger: east 2/30, north 1/7, west 2/20 -- ordered, tab-separared
wait_value "d: GROUP BY region count+sum ledger" "$PORT" \
    "SELECT region, COUNT(*), SUM(amount) FROM sr_orders GROUP BY region ORDER BY region" \
    "$(printf 'east\t2\t30\nnorth\t1\t7\nwest\t2\t20')"

# ---- e. rejection matrix: unsupported clauses fail LOUDLY ---------------
# MySQL 1235 with the exact clause-naming text from src/sql/parse/starrocks.rs.
expect_err "e: PARTITION BY rejected" "$PORT" \
    "CREATE TABLE sr_bad_part (k INT PRIMARY KEY) PARTITION BY RANGE(k) (PARTITION p0 VALUES LESS THAN (100))" \
    "1235" "StarRocks PARTITION BY"
expect_err "e: UNIQUE KEY model rejected" "$PORT" \
    "CREATE TABLE sr_bad_uniq (k INT PRIMARY KEY) UNIQUE KEY(k)" \
    "1235" "StarRocks UNIQUE KEY model"
expect_err "e: AGGREGATE KEY model rejected" "$PORT" \
    "CREATE TABLE sr_bad_agg (k INT NOT NULL) AGGREGATE KEY(k)" \
    "1235" "StarRocks AGGREGATE KEY model"
expect_err "e: PK model + ENGINE=columnar rejected" "$PORT" \
    "CREATE TABLE sr_bad_eng (k INT NOT NULL) PRIMARY KEY(k) ENGINE=columnar" \
    "1235" "ENGINE=columnar is not supported"
expect_err "e: DUPLICATE model + ENGINE=row rejected" "$PORT" \
    "CREATE TABLE sr_bad_row (k INT) DUPLICATE KEY(k) ENGINE=row" \
    "1235" "ENGINE=row is not supported"
# loud rejections have no side effects: none of the tables may exist (1146)
expect_err "e: rejected sr_bad_part was NOT created" "$PORT" \
    "SELECT COUNT(*) FROM sr_bad_part" \
    "1146" "doesn't exist"
expect_err "e: rejected sr_bad_eng was NOT created" "$PORT" \
    "SELECT COUNT(*) FROM sr_bad_eng" \
    "1146" "doesn't exist"
# append-only writes are rejected too (src/sql/exec/write.rs:235)
expect_err "f: UPDATE on columnar table rejected" "$PORT" \
    "UPDATE sr_events SET lvl = 'x' WHERE k = 1" \
    "1235" "append-only in this version"

# ---- f. columnar append + read-back in 3 batches ------------------------
expect_ok "f: CREATE sr_logs DUPLICATE model" "$PORT" \
    "CREATE TABLE sr_logs (k BIGINT NOT NULL, msg VARCHAR(16)) DUPLICATE KEY(k) DISTRIBUTED BY HASH(k) BUCKETS 3"
expect_ok "f: batch 1 (3 rows)" "$PORT" \
    "INSERT INTO sr_logs (k, msg) VALUES (1,'a'),(2,'b'),(3,'c')"
wait_value "f: count after batch 1" "$PORT" "SELECT COUNT(*) FROM sr_logs" "3"
expect_ok "f: batch 2 (4 rows)" "$PORT" \
    "INSERT INTO sr_logs (k, msg) VALUES (4,'d'),(5,'e'),(6,'f'),(7,'g')"
wait_value "f: count after batch 2" "$PORT" "SELECT COUNT(*) FROM sr_logs" "7"
expect_ok "f: batch 3 (3 rows)" "$PORT" \
    "INSERT INTO sr_logs (k, msg) VALUES (8,'h'),(9,'i'),(10,'j')"
wait_value "f: count after batch 3" "$PORT" "SELECT COUNT(*) FROM sr_logs" "10"

# ---- g. dual CLI parity: the official mysql client re-reads (b) ---------
UP_NAME=$(sql_scalar "$PORT" "SELECT name FROM sr_user WHERE id = 1")
RAW_NAME=$(sql_raw "$PORT" "SELECT name FROM sr_user WHERE id = 1")
assert_eq "g: mysql client reads the upserted value" "alice2" "$RAW_NAME"
assert_eq "g: both CLIs agree on id=1 name" "$UP_NAME" "$RAW_NAME"
wait_value "g: mycli count for sr_events" "$PORT" \
    "SELECT COUNT(*) FROM sr_events" "10"
assert_eq "g: mysql client count for sr_events" "10" \
    "$(sql_raw "$PORT" 'SELECT COUNT(*) FROM sr_events')"

# ---- h. multi-column PK model + DECIMAL value column --------------------
# PRIMARY KEY(k1, k2) is the composite-pk row store (W2.1): upserts
# dedup on the FULL tuple, DECIMAL(10,2) values are exact (W2.0), and
# DISTRIBUTED BY HASH accepts multi-column lists (parse/starrocks.rs
# ident_list + ddl.rs per-column existence check). DECIMAL only rides
# row stores -- the columnar engine rejects it (see the rejection
# matrix below).
expect_ok "h: CREATE sr_mpk composite PK model" "$PORT" \
    "CREATE TABLE sr_mpk (k1 BIGINT NOT NULL, k2 VARCHAR(32) NOT NULL, v DECIMAL(10,2) NULL) PRIMARY KEY(k1, k2) DISTRIBUTED BY HASH(k1, k2) BUCKETS 3"
assert_contains "h: sr_mpk in SHOW TABLES" "sr_mpk" \
    "$(sql_exec "$PORT" 'SHOW TABLES')"
expect_ok "h: seed sr_mpk (k1=1 twice, k1=2 once)" "$PORT" \
    "INSERT INTO sr_mpk (k1, k2, v) VALUES (1, 'a', 0.1), (1, 'b', 0.2), (2, 'c', 0.1)"
# same (k1,k2) again: latest wins on the full tuple
expect_ok "h: re-INSERT (1,'a') upserts" "$PORT" \
    "INSERT INTO sr_mpk (k1, k2, v) VALUES (1, 'a', 0.3)"
wait_value "h: upsert keeps exactly 3 rows" "$PORT" \
    "SELECT COUNT(*) FROM sr_mpk" "3"
wait_value "h: (1,'a') carries the NEW v (0.3 -> 0.30)" "$PORT" \
    "SELECT v FROM sr_mpk WHERE k1 = 1 AND k2 = 'a'" "0.30"
wait_value "h: (1,'b') keeps its own v" "$PORT" \
    "SELECT v FROM sr_mpk WHERE k1 = 1 AND k2 = 'b'" "0.20"
# decimal aggregation over the PK model rows: SUM at column scale
wait_value "h: SUM(v) is exact at the column scale (0.30+0.20+0.10)" "$PORT" \
    "SELECT SUM(v) FROM sr_mpk" "0.60"
wait_value "h: SUM(v) GROUP BY k1 ledger" "$PORT" \
    "SELECT k1, SUM(v) FROM sr_mpk GROUP BY k1 ORDER BY k1" \
    "$(printf '1\t0.50\n2\t0.10')"
# surface: both key columns PRI, v typed decimal(10,2)
assert_contains "h: DESCRIBE flags k1 PRI" "k1,bigint,NO,PRI" \
    "$(sql_raw "$PORT" 'DESCRIBE sr_mpk' | tr '\t' ',')"
assert_contains "h: DESCRIBE flags k2 PRI" "k2,varchar,NO,PRI" \
    "$(sql_raw "$PORT" 'DESCRIBE sr_mpk' | tr '\t' ',')"
assert_contains "h: DESCRIBE types v decimal(10,2)" "v,decimal(10,2),YES," \
    "$(sql_raw "$PORT" 'DESCRIBE sr_mpk' | tr '\t' ',')"

# ---- i. new rejections: composite-pk types & columnar DECIMAL ----------
# composite pk columns are narrowed to Int/VarChar/Date/DateTime
# (exec/ddl.rs): a DOUBLE key column is 1235 with the type named.
expect_err "i: DOUBLE in a composite pk rejected" "$PORT" \
    "CREATE TABLE sr_bad_dbl (k1 BIGINT, k2 DOUBLE, v INT) PRIMARY KEY(k1, k2) DISTRIBUTED BY HASH(k1) BUCKETS 1" \
    "1235" "composite PRIMARY KEY column 'k2' has type double"
# DECIMAL needs the row store: the columnar (DUPLICATE) engine has no
# decimal segment encoding (exec/ddl.rs DECIMAL guard).
expect_err "i: DECIMAL on a columnar table rejected" "$PORT" \
    "CREATE TABLE sr_bad_dec (k BIGINT NOT NULL, v DECIMAL(10,2)) DUPLICATE KEY(k) DISTRIBUTED BY HASH(k) BUCKETS 1" \
    "1235" "DECIMAL columns are not supported on columnar tables"
# loud rejections leave no table behind (MySQL 1146 on read)
expect_err "i: rejected sr_bad_dbl was NOT created" "$PORT" \
    "SELECT COUNT(*) FROM sr_bad_dbl" "1146" "doesn't exist"
expect_err "i: rejected sr_bad_dec was NOT created" "$PORT" \
    "SELECT COUNT(*) FROM sr_bad_dec" "1146" "doesn't exist"

e2e_finish "$SCENARIO"
