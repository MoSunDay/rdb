# COMPAT.sql.md — SQL data plane contract & deviation ledger (Rust-only)

Split out of `COMPAT.md` (2026-10-06, MySQL-gap M0-M5 doc sync) when the section
outgrew that file's size budget; conventions unchanged. The Go implementation never
had a SQL data plane, so every item here is a Rust-vs-**MySQL** contract. Module map:
`agents/rust/sql.md`; feature summary: `features/sql-dataplane.md`; program record:
`plans/2026-10-06-mysql-gap/` + `features/changelog/2026-10-06/mysql-m*.md`.

## SQL data plane (MySQL wire, Rust-only)

The Rust tree adds a SQL data plane the Go implementation never had. Summary of the
contract; module map lives in `agents/rust/sql.md`.

- **Access**: `mysql_bind` listener (opensrv-mysql; native-password only, `mysql_user`/
  `mysql_password` from config; any other account is rejected at handshake). `USE db`
  sets the session default database, validated against the known set (`rdb` — the
  implicit default, case-insensitive; anything else fails with MySQL 1049 "Unknown
  database 'x'"; a prepared USE rejects 1235). `DATABASE()` and `SHOW DATABASES`
  reflect it, and cosmetic SET
  (`sql_mode`, `wait_timeout`, ...) are tolerated no-ops; SET statements
  that change session/transaction semantics (autocommit, NAMES, TIME_ZONE, ROLE, ...)
  are REJECTED loudly (MySQL 1235) rather than silently ignored. `SET [SESSION]
  TRANSACTION ISOLATION LEVEL ...` is accepted and echoed per session by
  `@@transaction_isolation`/`@@tx_isolation` (default `REPEATABLE-READ`, MySQL's
  default — and factually the engine's only isolation, since snapshot reads are
  repeatable); the engine never changes behavior on it. `SELECT @@var` is intercepted
  server-side.
- **MVCC rows**: physical key `<slot>/ 0x20 table_id(BE u32) pk_key ts(BE !ts)` — newest
  version first per pk; value = header (0x01 live, 0x00 tombstone, 0x02 2PC-prepared) +
  null bitmap + typed payloads. Slot = `crc16(table_id BE ++ pk_key) % 16384` (same 16384
  slot space as RESP; SQL is not subject to RESP MOVED — routing is node-internal).
- **Timestamps**: node-local atomic oracle when the cluster is not ready; once
  `CLUSTER INIT` lands, block allocation through the raft leader (`sql_ts_cursor`,
  HTTP `/sql/ts?n=` on the leader, 4096-ts blocks, cursor persisted via raft BEFORE a
  block is served). `CLUSTER INIT` itself seeds the leader's binds into the raft
  `sql_nodes` registry through a second apply awaited inside the same `+done` reply
  window (region-metadata-style: peers resolve the ts authority the moment the FSM
  entry replicates), and the registration loop fast-polls (250ms) the `cluster_ready`
  edge. Intentional deviation (TSO discipline): a commit whose write set spans remote
  slot owners (2PC) FAILS FAST with MySQL 1213 (retryable) when the ts authority
  cannot lease a block above the write frontier — it never stamps locally-bumped GAP
  ranges, because no later cursor ride can cover them and newest-ts-wins would bury
  the write silently. Purely local writes keep the degraded fallback above the last
  known global watermark (monotonicity over availability of strict ordering); a
  same-node refill re-anchors them.
- **Transactions**: `BEGIN`/`COMMIT`/`ROLLBACK` snapshot isolation — reads pinned at the
  txn read_ts (registered with the oracle), writes staged in a per-session write set,
  first-committer-wins write-write validation (MySQL error 1213). DDL inside a txn is
  rejected. Disconnect rolls back. Autocommit statements skip staging (single batch).
  `SAVEPOINT`/`ROLLBACK TO`/`RELEASE SAVEPOINT` operate on the staged write set (a bare
  SAVEPOINT opens an implicit txn): a marker snapshots the whole write map + append-buffer
  lengths + latches held at that moment; `ROLLBACK TO` restores that state, keeps the
  marker (repeatable) and pre-marker latches, and releases post-marker latches with the
  undone writes; `RELEASE` drops the marker and every later one; a reused name shadows
  (case-insensitive, latest wins); unknown names are 1305.
  One transaction writes ONE engine: staging a row-store write after a columnar append
  (or vice versa) is rejected at the statement (MySQL 1235, "a transaction cannot mix
  row-store and columnar writes"); mixed reads across engines remain legal.
- **DDL atomicity**: a DDL statement's table-id allocation and its catalog mutations
  (table/index entries, schema, CREATE INDEX backfill) are decided inside ONE raft
  write-guard window (`catalog_txn`/`DdlPlan`) and applied together, so concurrent
  `CREATE TABLE`s across sessions can never collide on table ids — the loser observes
  the winner's committed name and fails with 1050 (table exists), not a duplicate id.
  A plan that drained no mutations is a no-op (`changed` flag), which keeps backfill
  from being skipped on re-parse paths.
- **DDL & session surface** (MySQL-gap M4): TRUNCATE TABLE keeps the schema byte-for-
  byte but allocates a FRESH table_id and tombstones the old one — physically DROP +
  same-definition re-CREATE, so ONE raft decision truncates cluster-wide (rows and
  secondary/unique entries are keyed by table_id and become unreachable immediately;
  MVCC GC reclaims them in the background; the AUTO_INCREMENT counter resets to 1 in
  the same window); rejected inside a txn (1235), never rollbackable. RENAME TABLE
  (and `ALTER TABLE ... RENAME [TO|AS]`) is a catalog-only rename — table_id and the
  physical key encoding are untouched (data/index zero-copy) and the
  `sql_sequence/<name>` counter moves verbatim (auto-increment CONTINUES across
  renames, possibly above the reserved batch — a MySQL-style gap accepted); the old
  name gets an id-less `""` tombstone (NEVER a dropped-id entry — the id stays live
  under the new name, so background GC cannot eat the renamed table) and 1146s immediately, including re-EXECUTE of prepared statements against it
  (execute re-parses the catalog, so it errors cleanly instead of hanging); rejects:
  occupied target 1050 (case-insensitive compare; case-only renames allowed), missing
  source 1146, multi-pair and cross-db forms 1235. `ALTER TABLE ADD [UNIQUE] INDEX` /
  `DROP INDEX`, `CREATE [UNIQUE] INDEX`, `DROP INDEX`, and inline `KEY`/`UNIQUE KEY`
  in CREATE TABLE all converge on the same index executor as CREATE-TIME indexes
  (still single-column per index — composite/prefix reject 1235, P2; two indexes
  sharing one column sweep each other's entries); every other ALTER operation names
  its "P2 deferred" refusal loudly. SHOW CREATE TABLE renders deterministic MySQL-
  style DDL from the schema (round-trips into an equivalent schema; ENGINE= reports
  InnoDB for row tables, columnar for columnar tables); SHOW DATABASES lists the
  implicit `rdb` plus the USE target (single-db model); `SHOW [GLOBAL|SESSION]
  VARIABLES [LIKE 'pat']` shares the 25-entry sysvar table with @@-queries — LIKE is
  case-insensitive with `%`/`_` wildcards HERE while data LIKE stays byte-wise, and a
  WHERE filter rejects (1235); SHOW STATUS answers only honest counters (`Uptime`),
  never fabricated load numbers. DATABASE()/SCHEMA()/USER()/CURRENT_USER()/
  SESSION_USER()/CONNECTION_ID() bind once per execution at the executor entry (a
  PREPARE before USE re-reads the new database on every EXECUTE; one snapshot per
  statement), keeping the expression evaluator session-free; `SessionInfo`
  (user, connection id) is threaded from the handshake through `SqlSession`; ids come
  from a per-process atomic counter seeded with epoch seconds (epoch + seq, wrapping,
  skipping 0, nothing persisted — a restart >= 1s apart never re-issues an id).
- **Locking reads**: `SELECT ... FOR UPDATE / FOR SHARE [OF tbl]` latches the matched
  (post-filter) rows in a node-local registry keyed `(table_id, pk)`. FOR UPDATE is
  exclusive, FOR SHARE composes; re-acquiring with the same owner — including a FOR
  SHARE -> FOR UPDATE upgrade inside one txn — is a no-op (MySQL own-lock upgrade).
  Acquisition is all-or-nothing per statement and conflicts fail FAST with MySQL 1205
  ("Lock wait timeout exceeded; try restarting transaction"): rdb never blocks, has no
  lock-timeout knob and no deadlock detection, and `NOWAIT`/`SKIP LOCKED` are rejected
  (1235). Explicit-txn latches release at COMMIT/ROLLBACK; autocommit locking reads are
  ephemeral (released at statement end). Multi-node clusters veto the statement (1235):
  latches are process-local and the gather path cannot lock remote bands — the statement
  is refused rather than silently degraded.
- **Secondary indexes**: `0x21` secondary (`slot/ 0x21 table_id col_pos key(val) pk`), `0x22`
  unique (`slot/ 0x22 table_id col_pos key(val)` -> pk). Index slot = `crc16(table_id ++
  col_pos)` so one index is contiguous in one slot band. NULLs unindexed. Unique is
  enforced at write/commit (1062); `CREATE UNIQUE INDEX` pre-checks existing rows (a race
  window vs concurrent writers is accepted and documented in-code). Single-column only.
- **AUTO_INCREMENT**: one integer pk column per table, and that column must be the
  entire (single-column) pk — a composite pk containing the auto column rejects with
  MySQL 1075. The counter is
  raft-replicated catalog state (`sql_sequence/<table>` = decimal next value; seeded at 1
  by CREATE, cleared by DROP), so ids survive restarts and leadership changes. Allocation
  is a serialized leader-only read-modify-write (same raft write guard as DDL — a
  non-leader INSERT that must allocate ids fails with "AUTO_INCREMENT allocation requires
  the raft leader"). Missing column / NULL / 0 auto-assign (MySQL default sql_mode: 0
  means auto); an explicit value >= the running floor bumps the floor to value+1
  immediately (later rows of the same statement continue above it); below-floor and
  non-positive explicit values are kept verbatim (except 0). Every auto-allocating
  statement reserves a batch of 64 ids ahead (gaps accepted, like MySQL); explicit-only
  statements persist exactly value+1. `LAST_INSERT_ID()` returns the first id the
  connection auto-generated and `LAST_INSERT_ID(n)` sets-and-returns n; because
  expression evaluation cannot see the session, the value additionally lives in a
  process-wide atomic mirror — connections in ONE process share it (a deliberate v1
  deviation). `LAST_INSERT_ID()` result metadata is BIGINT, like MySQL. The OK packet of a
  statement that ran an INSERT carries the session's last-insert-id in its
  `last_insert_id` field; every non-INSERT statement's OK packet carries 0 (resultset
  terminators do too — do not read the value after a SELECT).
- **Temporal types**: `DATE` = days since epoch (i64), `DATETIME`/`TIMESTAMP` =
  microseconds (i64); TIMESTAMP is a plain DATETIME alias (no time-zone semantics) and
  `(fsp)` is parsed and ignored (full microsecond precision; the fraction renders only
  when nonzero). Literals accept canonical `YYYY-MM-DD[ HH:MM:SS[.ffffff]]` and compact
  digit-only forms, plus MySQL's loose 1–2 digit month/day (`'2020-1-1'` works) with
  fraction digits beyond 6 truncating; Str <-> temporal coercion applies at write and in comparisons, and
  an Int compares as the compact numeric value. Garbage (and the MySQL zero date
  `'0000-00-00'`, which is unrepresentable) rejects the whole statement loudly with
  "Incorrect DATE value: '...'" with MySQL's own errno 1292. No temporal
  arithmetic; SUM/AVG over temporal is NULL. `NOW()`/`CURRENT_TIMESTAMP()`/`CURDATE()`
  are typed functions. Wire: text results are canonical strings; the binary protocol
  emits MYSQL_TYPE_DATE (4-byte) and MYSQL_TYPE_DATETIME (7/11-byte) cells and accepts
  string or binary date/datetime statement parameters (TIME stays 1235).
  Storage codec tags `0x06` (Date) / `0x07` (DateTime) appear in payloads and index
  keys. Rollout gate: SqlType travels in the catalog JSON, columnar segment
  footers/meta, and the ColumnarRows RPC — mixed-version clusters cannot decode a new
  catalog/segment/RPC, so nodes upgrade together.
- **Decimal type**: `DECIMAL(p,s)` / `NUMERIC(p,s)`, p 1..=38, s <= p, i128 fixed-point
  mantissa (effective digits are bounded by i128, not p). Literals and prepared
  parameters parse exactly — never through double. Arithmetic is exact: `+`/`-` align to
  the coarser scale, `*` multiplies mantissae and adds scales, `%` aligns, all checked
  (overflow is loud); `/` is MySQL-style long division — quotient at dividend
  scale+4 (`div_precision_increment=4`), remainder rounded half away from zero; any
  DOUBLE operand coarsens the whole expression to double. `Int / Int` stays INTEGER
  division — MySQL's `/` returns a decimal quotient (known divergence, listed with the
  SQL dataplane limitations); a decimal quotient needs a DECIMAL operand on either side. Decimal/Decimal (cross-scale)
  and Decimal/Int comparisons are exact in i128; comparisons against DOUBLE/strings go
  through f64/parse (explicit downgrade). `SUM(decimal)` stays DECIMAL at the column
  scale; `AVG` divides the exact sum at scale+4 — `AVG(int)` types as DECIMAL(38,4),
  `AVG(decimal)` as DECIMAL(38, scale+4); result-column metadata for decimal
  arithmetic/aggregates is MYSQL_TYPE_NEWDECIMAL (mislabeling them DOUBLE once made SDKs
  read the exact cell "0.60" as 0.6 and the binary encoder reject the cells — fixed).
  Writes coerce to the column scale (half away from zero); an integer part wider than
  p-s rejects the statement with MySQL 1292. Wire type is MYSQL_TYPE_NEWDECIMAL (text
  cells are canonical fixed-point strings). Storage codec tag `0x08` (payload and
  order-preserving fixed-width key encoding — byte order equals value order, negatives
  included — so DECIMAL columns take secondary/unique indexes and ORDER BY). Trims
  (1235, loud): a DECIMAL column cannot be a primary key (single-column pks too), and
  DECIMAL columns are rejected on columnar tables (segment pages have no decimal
  encoding).
- **Composite primary keys**: `PRIMARY KEY(a,b,...)` is accepted. Column types are
  narrowed to what concatenates unambiguously in an order-preserving key: TINYINT/
  SMALLINT/INT/BIGINT (all engine-internal Int), VARCHAR, DATE, DATETIME — BOOL,
  DOUBLE, BLOB and DECIMAL reject with 1235. Multi-column pks encode as the
  declaration-ordered concatenation of per-column key codecs (var-length components
  NUL-escape 0x00 -> 0x00 0xFF and terminate with 0x00, so the split is unique and byte
  order equals tuple order); secondary/unique index entries, 2PC write sets, locking-read
  latches and row probes all carry the full pk tuple. AUTO_INCREMENT must still be the
  ENTIRE single-column pk: a composite pk containing the auto column rejects with MySQL
  1075. Catalog shape: `TableSchema.pk` widened String -> Vec<String>, serialized as an
  array; old catalog JSON that carried the bare string `"pk":"id"` deserializes as the
  one-element vector (`de_string_or_vec`), and the new DECIMAL `SqlType` variant is
  additive — old binaries simply never see either (they cannot decode the new shape),
  which is exactly the co-upgrade gate above: mixed-version clusters are unsupported,
  nodes upgrade together in one batch. Rehearsal evidence:
  `scrtips/e2e_scenarios/upgrade_rehearsal.sh` (stop-the-world c22ff37 -> HEAD swap,
  94 assertions, run twice) — see `scrtips/e2e_scenarios/RESULTS.md`.
- **Local ts floor (single-node durability)**: every durable batch that stamps MVCC
  records also stamps ONE store-reserved key `\x00sql_ts_floor` (the batch's highest ts,
  riding the batch's existing fsync — no extra write); boot replays it into the oracle
  (`advance_to`, on both the normal and backup listener paths), so a kill -9 restart can
  never run the clock backwards (a backwards clock makes every previously committed row
  invisible and shadows same-pk rewrites with stale higher-ts versions). The key has no
  `"N/"` slot prefix and classifies as Foreign: FLUSHDB preserves it and DBSIZE/INFO
  never count it. In-place upgrade from a pre-floor binary: when the key is ABSENT, the
  first boot runs ONE full-keyspace scan (row-version keys with a crc16 slot
  cross-check, plus columnar segment commit_ts from the 0x23 metas), takes the max ts
  and stamps the key immediately — no later boot ever scans again (fresh stores return
  instantly). A mid-scan iterator error keeps the partial max (still a floor) and logs.
  Cluster mode is fenced by the raft-replicated `sql_ts_cursor` either way; the floor
  is the single-machine equivalent and also covers a cluster node's local writes before
  the cluster forms.
- **Planner**: sargable `=`/`IN`/`BETWEEN` on an indexed column -> pk lookup (>1000 pks or
  no index -> SeqScan). In cluster mode the index path is disabled (v1) and EXPLAIN shows
  `Gather(bands=N)` over `SeqScan`.
- **Query language**: `UNION [ALL]` / `INTERSECT [DISTINCT|ALL]` / `EXCEPT [DISTINCT|ALL]`
  over SELECTs (column arity checked, INT|DOUBLE columns widen to DOUBLE, DISTINCT
  dedups with NULLs equal, ALL is multiset min / subtraction, result names from the
  left operand; INTERSECT binds tighter than UNION/EXCEPT and same-precedence chains
  fold left — standard SQL), non-recursive `WITH` CTEs (case-insensitive, later
  definitions shadow, optional column alias list), derived tables
  (`FROM (SELECT ...) alias`), scalar / `IN` / `EXISTS` subqueries — uncorrelated ones
  hoist and materialize before execution, correlated ones run a defer-then-bind second
  pass once the outer FROM is materialized (outer references substituted as literals
  per DISTINCT outer key, memoized; evaluation itself stays storage-free: an empty
  scalar is NULL, >1 rows is a 1242-style error, `NOT IN` over an empty member set
  keeps every row and is never true once a NULL member appears) — and FROM-less SELECT
  (`SELECT 1`) / `FROM DUAL` are supported; every operand runs through the normal
  pipeline, so in a ready cluster each operand gathers cluster-wide before the
  coordinator composes. Trailing ORDER BY (ordinals allowed)/LIMIT/OFFSET apply to the
  whole compound. ORDER BY / GROUP BY bare unsigned integers are 1-based output-column
  ordinals (out-of-range / 0 -> MySQL 1054 wording; an ordinal pointing into a `*` or
  `?` projection is a loud 1235; `'1'`, `-1`, `1+1` stay constant keys), and ORDER BY /
  HAVING bare identifiers resolve SELECT aliases first (case-insensitive, the alias
  beats a same-named FROM column, no chained substitution) before FROM scope (unknown
  -> 1054); GROUP BY bare identifiers do the inverse — the FROM column wins and the
  SELECT alias is only a fallback. `LIMIT ?` / `OFFSET ?` placeholders are validated at bind time
  (non-negative integer, else 1064 — prepare does not check; the binary protocol binds
  limit-then-offset, the text order of `LIMIT ? OFFSET ?`). The ambiguous comma form
  `LIMIT ?, ?` with TWO placeholders is a loud 1064 (its text order is offset-first,
  which the shared limit-then-offset binder would silently swap); single-placeholder
  comma forms (`LIMIT ?, 5` / `LIMIT 5, ?`) bind positionally and stay supported. Loudly rejected (MySQL 1235): MINUS, `BY NAME` set quantifiers,
  `WITH RECURSIVE`, LATERAL derived tables, derived tables without an alias, correlated
  subqueries in JOIN conditions, outer references inside derived-table bodies, and the
  two-placeholder `LIMIT ?, ?` comma form (bind-order ambiguity — see LIMIT above);
  skip-level outer references honestly name the unbindable column (1054). Expressions
  follow SQL three-valued logic:
  comparisons with NULL yield NULL, `NOT NULL` is NULL, and `IN`/`NOT IN` short-circuit
  on the first equality hit, otherwise return NULL once any NULL operand was seen
  (`2 IN (NULL, 1)` is NULL, not FALSE — as in MySQL). Scalar `length()` counts BYTES
  while `char_length()` counts characters.
- **Expressions & functions** (MySQL-gap M1): 51 scalar function entries (incl.
  aliases) behind a pure family dispatch `control -> string -> numeric -> datetime`
  (`exec/func/`, no registry state): strings CONCAT/CONCAT_WS/SUBSTRING/LEFT/RIGHT/
  LPAD/RPAD/REPEAT (exact result byte length pre-checked against the 1<<24 wire
  cap — over -> NULL, no allocation)/LOCATE (NULL-first)/INSTR/POSITION/REPLACE/TRIM
  (no-remstr forms default to ' ')/REVERSE/HEX/UNHEX/UPPER/LOWER/LENGTH/CHAR_LENGTH; numerics ROUND (exact half-away-from-zero on DECIMAL incl.
  negative-digit scaling, f64 on Double)/CEIL/CEILING/FLOOR (call AND keyword forms;
  the `TO <unit>` form rejects)/TRUNCATE/MOD/POW/POWER/SQRT (SQRT negative -> NULL; POW exponent outside
  [-30, 30] or non-finite result -> NULL — never a clamped wrong value)/SIGN/
  GREATEST/LEAST/ABS; datetimes DATE/YEAR/MONTH/DAY/DAYOFMONTH/HOUR/MINUTE/SECOND
  (malformed spelling -> NULL)/DATE_ADD/DATE_SUB/ADDDATE/SUBDATE (INTERVAL units
  YEAR..MICROSECOND with month/leap-day clamping; time units promote DATE to DATETIME;
  the `d +/- INTERVAL n unit` operator form works too)/DATEDIFF/DATE_FORMAT/
  UNIX_TIMESTAMP/FROM_UNIXTIME/NOW/SYSDATE/CURDATE/CURTIME; control IF/IFNULL/NULLIF/
  COALESCE (lazy — untaken branches never evaluate)/VERSION/LAST_INSERT_ID. Plus
  searched and simple CASE (short-circuit, NULL never matches), CAST/CONVERT
  (`CONVERT ... USING` only utf8/utf8mb4), 9 new operators — `<=>`, REGEXP/RLIKE,
  `& | ^ << >>` (u64 semantics: negatives shift as two's-complement words, shift >= 64
  -> 0, no masking), logical XOR — and the GROUP_CONCAT aggregate ([DISTINCT] expr
  [SEPARATOR s], several args concatenate): skips NULLs, defaults to `,`, joins in
  first-seen-per-group order (an all-NULL group is the empty string); the inner
  ORDER BY form rejects (1235, P2). Arity is checked at PREPARE against a signature
  table (MySQL 1582 wording). Result-column metadata comes from a pure name->SqlType
  table (CHAR_LENGTH->LONGLONG, ROUND(DECIMAL)->NEWDECIMAL(d), NOW->DATETIME,
  CURDATE->DATE, UNHEX->BLOB, ...). Intentional deviations: comparisons, LIKE, and
  REGEXP are byte-wise case-sensitive with NO collation (MySQL's default utf8mb4 is
  case-insensitive) and REGEXP speaks the Rust `regex` dialect (no backreferences/
  lookaround); clock functions run UTC with no session time zone; GREATEST/LEAST
  reject mixed string/numeric arguments and ROUND/SIGN reject string arguments loudly
  (MySQL would attempt implicit casts).
- **Column metadata**: column definitions (SHOW COLUMNS and the wire protocol) carry
  NOT_NULL_FLAG / PRI_KEY_FLAG from `ColMeta.nullable`/`ColMeta.primary`; a
  DUPLICATE-model schema-only pk gets no key flag.
- **DML conflict paths** (MySQL-gap M2): `INSERT ... ON DUPLICATE KEY UPDATE` reports
  MySQL's affected rows 1/2/0 (insert / conflicting row changed / identical — the
  whole row is compared before any write; multi-row statements accumulate, e.g.
  [new, changed, identical] -> 3). Inside assignments a bare column reads the EXISTING
  row while `VALUES(col)` reads the INCOMING row (`c = c + VALUES(c)` works) and
  columns absent from the assignment list keep the existing row's value; `VALUES()` is
  legal only there (1064 in the INSERT tuple, 1235 elsewhere). The conflict snapshot
  is taken purely before anything lands (autocommit: frontier-synced now; txn: pinned
  read_ts merged with the txn's staged writes), probed pk-first then unique indexes in
  column order; per-unique-index value->owner maps are maintained as a live overlay
  (every decided write releases/releases-and-claims its entries), so later rows of one
  statement see earlier rows' effects exactly — a unique value freed by an earlier
  ODKU/REPLACE row no longer conflicts, an ODKU/REPLACE that moves the pk onto another
  live row fails with 1062 (plain `UPDATE` moving the pk onto a live row does too);
  an ODKU that changes the pk itself migrates row + index entries in the same write.
  REPLACE deletes the pk plus every unique-index hit (deduped) then
  inserts — affected = deletions + 1. `INSERT ... SELECT` materializes the source
  under one snapshot before any write (same-table read sees the pre-statement
  snapshot; in cluster the SELECT gathers once and the row writes go 2PC — it works
  everywhere ODKU/REPLACE do not). `INSERT ... SET` normalizes to a named single-row
  VALUES. AUTO_INCREMENT ids are allocated for every incoming row BEFORE the conflict
  decision, so an ODKU update branch still burns ids (MySQL's gap semantics). Columnar
  tables reject ODKU/REPLACE/INSERT..SELECT with 1235 (append-only, same gate as
  UPDATE/DELETE). Cluster mode rejects ODKU/REPLACE loudly — see the MySQL-gap ledger
  below. `UPDATE`/`DELETE` take trailing ORDER BY plus `LIMIT <n|?>` (a single value,
  no OFFSET); their `?` placeholders bind in statement text order
  SET -> WHERE -> ORDER BY -> LIMIT.
- **2PC writes**: any node accepts DML; the coordinator groups the pre-encoded batch by
  slot-band owner, PREPAREs (0x02 headers + unique entries + durable participant marker,
  one atomic RocksDB batch per participant), durably records the decision, then DECIDEs
  (commit flips 0x02->0x01 + applies secondary index entries; abort deletes). A commit
  whose outcome cannot be persisted fails CLOSED: no Decide leaves the coordinator
  (best-effort abort broadcast instead) and the client gets a retryable WriteConflict.
  In-doubt markers resolve via `/sql2pc/status?id=` on the coordinator, presumed-abort
  after a 60s lease; the answer carries ONLY the requesting node's mapped index ops
  (outcome records map ops per node -- a participant under its own bind; `own_ops` is
  local-replay state and never crosses the wire). Readers never see 0x02 rows.
- **Scatter-gather reads**: single-table scans fan out per slot band over the internal
  `sql_rpc_bind` TCP protocol (length-prefixed JSON), merged at the coordinator and then
  filtered/ordered/aggregated there. A dead band owner fails the query loudly (no partial
  results). JOINs materialize every side gather-aware (row tables per band, columnar tables
  to every member; `Gather(join)` in EXPLAIN) and run the shared nested loop on the
  coordinator -- a join silently reading only the local node's slice was a correctness bug
  and is fixed. Index paths stay local-only (v1).
- **GC**: a 30s sweep deletes versions at or below the oracle watermark except each pk's
  newest (live) anchor; tombstone anchors take their whole prefix with them. Prepared
  (0x02) versions are never swept. DROPPED tables are reclaimed too: every node's sweep
  reads the raft catalog's dropped-id set (ids retire under id-keyed side entries
  `sql_dropped/<id>`; the name-keyed tombstone is an id-less `""` so a same-name
  recreate — TRUNCATE's Drop+Put, DROP+re-CREATE — can never erase the record, and
  RENAME's old-name tombstone never marks the still-live id as dropped; pre-upgrade
  decimal-valued tombstones are still honored) and deletes ALL versions and
  `0x21`/`0x22` index entries of those ids —
  ids are never reused (allocation is max(live, dropped)+1, monotone across restarts),
  so a recreated table can never alias orphaned rows and the bytes are pure garbage.
- **MySQL-gap deviation ledger** (2026-10-06, M0-M5 — intentional deviations, marked):
  (1) a plain INSERT hitting a duplicate pk silently upserts (last-writer-wins — the
  StarRocks PRIMARY KEY import path depends on it) where MySQL reports 1062; ODKU and
  REPLACE are the explicit conflict paths and unique-index hits still reject 1062;
  (2) comparisons/LIKE/REGEXP are byte-wise case-sensitive, no collation; (3) `Int /
  Int` stays integer division (MySQL's `/` yields a decimal quotient — a DECIMAL
  operand on either side gives the decimal path); (4) ODKU and REPLACE — including
  ODKU on an INSERT..SELECT — are loudly rejected with 1235 ("not supported in
  cluster mode") from EVERY node whenever the write would go 2PC
  (`cluster_spans_remote`): the conflict decision needs the existing row and the coordinator-local
  snapshot cannot see remote slot owners; a correct cluster conflict read is a
  per-key gather before 2PC, beyond the point-read RPC budget (plan decision 1b —
  single-node and single-band-cluster writes are unaffected, plain INSERT..SELECT
  stays available); (5) GROUP_CONCAT has no inner ORDER BY. Deferred on purpose (P2,
  tracked in `plans/2026-10-06-mysql-gap/gap-matrix.md`): composite/prefix secondary
  indexes, ALTER ADD/MODIFY/DROP COLUMN (schema migration is its own project), WITH
  RECURSIVE, window functions, KILL, multi-statement batches. Protocol follow-up
  CLOSED (2026-10-08, was the M5 open follow-up — see
  `features/changelog/2026-10-08/mysql-m5-prepared-numeric-bind.md`): the prepared
  binary protocol is type-tagged by the announced column while the static result
  typing is best-effort (a `?` placeholder types as VAR_STRING until EXECUTE binds),
  so a runtime cell can disagree with the promised column. Such cells now encode IN
  the announced column's wire form wherever a faithful spelling exists
  (`src/sql/front/conv_bin.rs` compatible coercion: a numeric bound into a
  text-typed placeholder column ships as its canonical text, byte-identical to the
  text protocol; numerics into DOUBLE/FLOAT, Date<->DateTime cross forms, integer
  width narrowing likewise). A combination with no faithful spelling (e.g. a string
  cell against a numeric column, NULL against an announced NOT NULL column) is
  pre-flighted before the resultset starts and answers a loud 1292 ERR packet —
  the connection survives instead of dying on a mid-row encoder io error.
- **Numeric overflow & integer widths** (2026-10-07, mysql-hardening H0 review 问五 —
  intentional deviations, marked; numbering continues the ledger above): (6) integer
  overflow travels the WRONG error channel: engine integer arithmetic — Add/Sub/Mul/
  Neg/ABS, integer `SUM`, and `MIN / -1` — is fully checked and errors LOUDLY with
  MySQL-1690 WORDING ("BIGINT value is out of range in '...'"), but the wire errno/
  SQLSTATE ride the 1292 / 22007 channel (ER_TRUNCATED_WRONG_VALUE) instead of
  MySQL's 1690 / 22003 — clients keying on errno will misclassify the error;
  (7) `SUM(decimal)` mantissa overflow rejects with 1235 (NotSupported, "decimal SUM
  overflow") instead of MySQL's out-of-range wording; (8) `SUM(double)` may saturate
  to ±inf silently (the f64 path has no loud overflow check); (9) integer column
  types — ALL widths TINYINT/SMALLINT/INT/BIGINT — collapse to ONE signed 64-bit
  engine Int: declared display width and per-width ranges are not enforced (a
  TINYINT column silently stores 200), and UNSIGNED integer column types (e.g.
  BIGINT UNSIGNED) are rejected LOUDLY with 1235 at CREATE TABLE (sqlparser's
  Unsigned variants fall to the unsupported catch-all), so there is no unsigned
  domain anywhere — the silent part is only the width collapse, the UNSIGNED trim
  is an explicit reject.

