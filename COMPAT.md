# COMPAT.md — Rust `rdb` vs Go `rdb` compatibility notes

The Rust implementation is byte-compatible with the Go implementation on the **RESP data plane**
and the Raft **HTTP API**. The Raft **TCP wire protocol is intentionally different** (openraft
JSON framing vs hashicorp msgpack), so Go and Rust nodes cannot join the same raft cluster.

## Critical build requirement (tokio LIFO-slot freeze)

`.cargo/config.toml` sets `rustflags = ["--cfg", "tokio_unstable"]` so that
`tokio::runtime::Builder::disable_lifo_slot()` in `src/main.rs` compiles and takes effect.

Why this exists: with the tokio multi_thread runtime's default LIFO slot, this workload suffers
lost-wakeup freezes (~6s stalls, and multiples thereof) — tokio-rs/tokio#4941 family. Reproduced
on a fully idle 3-node cluster (followers freeze too), so it is inherent to openraft + LIFO slot
scheduling, not to write load. Evidence:
- current_thread runtime: zero freezes over 8+ minutes of hammering.
- multi_thread + `disable_lifo_slot()`: 2000+ raft writes, 937-write/145s soak, zero slow
  responses (>1s), zero errors, zero 1s-beacon gaps on any node.
- HA drill: kill -9 leader → follower elected in ~6s, writes OK, restarted node rejoins and
  catches up.

If you build the binary without `.cargo/config.toml` in scope (e.g. building from outside the
repository root), set `RUSTFLAGS='--cfg tokio_unstable'`. Since M0 this is enforced at compile
time: `src/build_guard.rs` fails any `full`-feature build that lacks the cfg (`compile_error!`);
the store-only slice (`--no-default-features --features store`) is exempt (it never spawns the
rdb runtime), and `[build] rustdocflags` mirrors the cfg so doc-tests compile too. CI proves
both directions: a `RUSTFLAGS=''` full build must fail, the store slice must still build. At startup the binary now
logs one line stating whether the cfg took effect (`tokio LIFO slot: disabled ...`) or not
(`tokio LIFO slot: ENABLED (DANGER: ...)`) — check it after any build-pipeline change that
overrides RUSTFLAGS. Without the cfg, the code falls
back to a multi_thread runtime WITH the LIFO slot (freezes return); the escape hatch
`RDB_CURRENT_THREAD=1` switches to the current_thread runtime, which is freeze-free but
single-threaded. `RDB_WORKER_THREADS=N` tunes the worker pool size (default: Go's NumCPU
parity).

## Intentional fixes of Go bugs (byte-incompatible by design)

1. **MSET odd arg count**: Go silently ignored the trailing key; Rust returns an error and
   stops applying the pair list.
2. **DEL return value**: Go always returned `:1`; Rust returns the real count (0/1 per key).
3. **ClusterReady semantics**: Go checks `len(joined_string) > 2`; Rust checks "parsed stable
   instances list non-empty". Diverges only for a single 1-char instance name (not realistic);
   otherwise equivalent.
4. **DBSIZE/Size**: Go counted store entries directly; Rust reads the RocksDB
   `rocksdb.estimate-num-keys` property (approximate, O(1)).
5. **QUIT reply** (BREAKING, approved): the Go fork wrote `+PONG` then `+OK`;
   Rust replies exactly one `+OK` before closing, like Redis.
6. **Missing key argument / empty multibulk** (BREAKING, approved): Go's
   unconditional `cmd.Args[0]`/`cmd.Args[1]` indexing surfaced as
   `-fatal error: runtime error: index out of range ...`; Rust replies the
   Redis-standard `ERR wrong number of arguments for '<command>' command`
   (empty name for `*0`). Handler panics still reply `fatal error: <panic>`.
7. **RAFTGET value framing** (BREAKING, approved): Go wrote the value as a
   RESP simple string (a CRLF-containing value corrupts the frame); Rust
   replies a bulk string. No latency sample is recorded on the arity-error
   or panic paths (Go observes only after the handler returns).
8. **Empty hash tag means no tag** (BREAKING, approved): `foo{}bar` now hashes
   the WHOLE key, as Redis does. Go hashed the empty tag, pinning every such
   key to slot 0 (CRC16("")==0), so existing empty-tag keys change slot.
9. **Slot coverage when 16384 % N != 0** (BREAKING, approved): the LAST node
   owns the leftover slots through 16383, so every slot has exactly one owner
   and bands stay disjoint. Go matched no node for the remainder and served
   those slots locally on whichever node received the request.
10. **Single-node `cluster nodes`/`cluster slots` range** (BREAKING, approved):
   reports the full `0-16383`. Go rendered the last node as `end+1..16383`,
   which for N=1 omitted slot 0 (`1-16383`).
11. **`/join` & `/depart` auth failure is `401`** (BREAKING, approved): a wrong
   raft token now answers `401 unauthorized` instead of the Go fake `ok`; the
   outgoing join URL percent-encodes query values so tokens containing
   `&`/`+`/`%` reach the peer intact. The mux-held membership section is also
   bounded by a 30s timeout (an unreachable peer can no longer wedge the
   control plane), and a departed voter is fully removed (openraft
   `change_membership(.., false)`, no lingering learner).

12. **`migrate task` is now a slot-migration orchestrator** (BREAKING, approved):
    `migrate task <slot> <src> <dst>` drives the redis-cli `--cluster reshard`
    protocol (MIGRATING -> IMPORTING -> GETKEYSINSLOT/MIGRATE drain -> NODE ->
    STABLE, node-ids are `md5_with40(addr)`). The raft `migrate_task` value is
    a single JSON task document (`migrate list` returns it); Go stored an
    underscore-joined task string. The data plane gained real
    `MIGRATE host port "" 0 timeout KEYS ...` + `RESTORE` transport
    (absolute-ms TTLs, `ABSTTL` accepted; `migrate`-reply texts differ from
    Go's registration-only replies).

## Preserved Go quirks (byte-compatible)

- `MGET`/`MSET` route by the **first key only** (all keys go to the first key's node).
- `cluster test` returns the hardcoded literal `-MOVED 5465 127.0.0.1:32681`.
- Slot range routing uses inclusive upper bound: `slot <= (i+1)*per`, `per = 16384/len`.
- `migrate task` overwrites `migrate_task` unconditionally (the value is a
  JSON task document since the orchestration rewrite, see deviation 12).
- `epoch = term + commit_index` concatenated as strings.
- Cluster-not-ready error text contains the Go typo: `instanes01`.
- Unknown command reply: `ERR unknown command '<raw-arg-bytes>'` (first arg verbatim).
- `AUTH` accepts exactly 2 args (`AUTH <token>`), reply `+OK` / `ERR: NOAUTH`.
- `MOVED <slot> <addr>` redirect format; cross-node requests redirect on the first-key slot.
- HA peer probe: plain TCP connect with 5s timeout, every 5s; self-recovery guarded by
  `len(dead)==2`; probe-only (no auto failover writes beyond backup_target_map semantics).
- Node description format: `<RaftTCPAddress> [<State>]`.

## Typed record physical encoding (Rust data plane)

The Rust tree stores every non-string type under a derived-key scheme the Go implementation does
not share (Go keeps only raw pebble keys + `slot/` prefix; on-disk stores are NOT interchangeable):

```text
data key   = <slot_prefix> ++ <kind:u8> ++ <key_len:u32 BE> ++ <user_key> [++ elem suffix]
value      = LEB128 varuint expire_ms (0 = no TTL) ++ payload
expire idx = <slot_prefix> ++ 0xFD ++ <expire_ms:u64 BE> ++ <data key from kind on>
```

- Kind registry lives in `src/ds/codec.rs` (0x00 raw string .. 0x12 vectorset elem).
- EXCEPTION: kind 0x00 raw STRING keeps the legacy `<prefix> ++ <key>` bare layout (no envelope),
  so pre-TTL databases keep working; the first EXPIRE migrates the record to kind 0x01.
- Classification rule during scans: a physical key whose first post-prefix byte is `<= 0x12` or
  `== 0xFD` reads as typed; a legacy raw string starting with such a byte is misread (accepted
  collision, raw strings written after this change start with an ordinary byte).
- Family deletes use ONE RANGE PER KIND (`family_delete_ranges`) -- a single family-wide span
  would swallow other keys' records because the kind byte sorts first.

## Intentional deviations (documented, not byte-compatible)

- **Raft wire protocol**: openraft JSON frames with u32 big-endian length prefix over TCP,
  replacing hashicorp msgpack. Go↔Rust clusters cannot mix; data plane unaffected.
- **Node IDs**: openraft requires numeric IDs; Rust derives a deterministic u64 from the node
  address (md5-based, first 16 hex chars). The address remains the human-visible identity in
  `raft nodes`, membership display, and all logs.
- **Timers**: heartbeat 500ms / election 1000–2000ms as in Go config; openraft tick granularity
  is `heartbeat * 3 / 2` = 750ms internally.
- **Apply wait**: `raft_apply` blocks the caller (std mpsc + 6s timeout) for Go WaitGroup
  parity; openraft applies asynchronously, typical latency ~ms.
- **ForwardToLeader mapping**: follower `raft set` maps openraft's `ForwardToLeader` error to
  the Go string `internal error err: not leader` (variant match, not string sniffing).
- **Join ordering**: Rust binds RESP + HTTP *before* issuing the join request (so peers can
  reach this node immediately); Go joins first. Join response must be exactly `ok`, else the
  process exits (fail-fast, same as Go). Concurrent `/join`/`/depart` on one server are
  serialized by a membership mutex spanning the whole add_learner → read-voters →
  `change_membership` sequence, so simultaneous joins are safe (fixed: previously two
  overlapping `change_membership` calls raced in openraft, one got `internal error` and its
  joiner process exited — the old workaround was staggering node starts ~3-5s apart).
  Bootstrap/`RAFT_JOIN_ADDR` semantics are identical,
  including the silent skip when `RAFT_JOIN_ADDR` is unset on a fresh data dir.
- **Storage fsync**: RocksDB WAL defaults vs Go pebble/bolt fsync-per-commit — durability
  windows are comparable but not bit-identical.
- **Monitor**: Prometheus text format and metric/label names match Go's collector.
- **Backup listener is actually read-only**: Go's `BackupServer` (mode "backup") executed
  whatever it was sent; Rust gates the `backup_bind` listener with the Redis-standard
  `-READONLY You can't write against a read only replica.` for every non-read command
  (allowlist: `src/command/readonly.rs`; enforced in `command::dispatch` — which EXEC replays
  re-enter — and at MULTI queue time, where a write also marks the transaction dirty →
  EXECABORT). `XIDLE` is denied in all forms: its `XIDLE <stream> <secs>` form persists
  stream meta + TTL entries (`lite/append.rs`) and the gate is name-based, so the bare
  query form is denied too. Lazy-expiry purge on read paths still runs on the backup
  listener — deleting already-TTL-dead data is the same semantics as the backup
  active-expire sweep, not a client-visible write. The backup store additionally gets
  its own active-expire sweep
  (`spawn_active_expire` on the backup `Shared`), so keys landing there vanish on schedule
  without needing a read; previously only the normal listener's store was swept.
- **Lite Mode (RocketMQ-style, rdb extension)**: parent topics with dynamic per-group queues
  exposed through Streams-verb commands (XADD/XLEN/XRANGE/XTRIM/XDEL/XIDLE/XREAD/XREADGROUP/
  XACK/XGROUP [CREATE|DESTROY|CREATECONSUMER|DELCONSUMER]/XPENDING/XCLAIM/XAUTOCLAIM/
  XINFO [STREAM|GROUPS|CONSUMERS|TOPICS|LITE]/XPICK). This is not a Redis Streams emulator:
  - PEL is persisted on the reserved KIND_STREAM_PEND (0x0F) window: pend rows
    (`group ++ 0x00 ++ id16BE`, id-ordered) plus a consumer registry
    (`group ++ 0x01 ++ name`); values = expire envelope + JSON
    `{consumer, delivered_ms, times_delivered}`. Delivery (`XREADGROUP ... >`) writes PEL
    rows synchronously in one latched WAL batch (same durability as entries) and
    auto-registers the consumer on first delivery; XACK deletes PEL rows atomically with
    the watermark persist. A kill -9 restart rewinds delivered to the committed watermark
    (kind-0x0E record) and redelivers everything still pending — at-least-once semantics;
    client idempotency is the contract, and the 200ms offset flusher window can only cause
    redelivery, never loss.
  - XREAD/XREADGROUP accept balanced multi-stream STREAMS lists (COUNT is a per-stream
    quota); a blocking all-`>` XREADGROUP parks ONE waiter registered under every
    `>`-stream meta key (any stream waking it serves the read).
  - XACK's reply counts WATERMARK advancement, not pending rows removed (an older-than-
    watermark id still acks `:0` while its PEL row is deleted). The committed watermark
    is a Kafka-style committed offset: it only advances over the CONTIGUOUS acked prefix
    (an ack beyond the first surviving PEL row sticks at `head_after_ack` and is NOT
    remembered — such ids may be redelivered; duplicates, never loss). The group
    record (kind 0x0E) is persisted synchronously only when the committed watermark
    actually advances.
  - XCLAIM supports FORCE / JUSTID plus the delivery hints IDLE/TIME/RETRYCOUNT in any
    order interleaved with the id list (P3 backfill, 2026-10-07; LASTID remains a syntax
    error): each hint applies to every successfully claimed PEL row (a JUSTID claim
    writes them too — an ownership move is still a PEL write); IDLE backdates
    delivered_ms = now-idle (clamped at 0), TIME parks it at the given wall clock,
    RETRYCOUNT overwrites times_delivered; delivered_ms stays the single source of the
    idle clock (XPENDING columns, the min-idle gate, the idle redelivery sweep), so a
    backdated row becomes min-idle eligible that much sooner. XAUTOCLAIM returns the
    Redis>=7 3-element reply (with deleted-ids),
    COUNT defaults to 100 with a 10x scan cap. For ORDERED groups both are HEAD-ONLY:
    only the PEL head (smallest pending id) can transfer queue ownership; a failed
    min-idle claim does not flip ownership, and FORCE on an id beyond the head is
    suppressed (the head still wins).
  - XGROUP CREATE accepts `ORDERED [INFLIGHT <n>]` (rdb extension, calibrated to
    Kafka's ordering model): an ordered group's queue is owned EXCLUSIVELY by one
    consumer at a time — in-memory lease (default 30s, no coordinator); a fenced-out
    or deposed consumer's `XREADGROUP ... >` delivers nothing (empty `*-1`, blocked
    readers re-park), a full in-flight window likewise delivers nothing until acks
    free slots, and an idle-expired lease migrates to the next asker with an epoch
    bump (takeovers wake parked contenders via the stream meta key). INFLIGHT is the
    prefetch knob (1 = strict serial, the default; requires ORDERED); ownership state
    is in-memory only — a restart drops it along with all connections.
  - XINFO GROUPS replies 7 field/value pairs (14 elements): name, last-delivered-id,
    committed-id, ordered, inflight, owner (nil when unowned), epoch (0 for unordered
    groups). PEL rows carry the owning epoch for observability.
  - XGROUP CREATE additionally accepts `MAXDELIVERY <n>=1` and `DLQ <name>` (orthogonal
    to ORDERED; DLQ requires MAXDELIVERY): the delivery that would push times_delivered
    past `n` dead-letters the row in ONE WAL batch — PEL row deleted, entry re-queued
    into the DLQ stream (original fields + trace fields `__dlq_group`/`__dlq_consumer`/
    `__dlq_times`/`__dlq_src`), watermark advanced as an XACK would; default target
    `<stream>/dlq`; ordered groups transfer only the PEL head; metrics: `rdb_lite_dlq_depth` + `rdb_lite_messages{op="dlq"}`.
  - Optional idle auto-redelivery via `lite.redelivery_idle_ms` (default 0 = sweep
    OFF): a 200ms sweep re-hands idle PEL rows to their current consumer (times+1,
    delivered_ms refreshed), over-MAXDELIVERY rows into the DLQ transfer; ordered
    groups sweep only the head; `rdb_lite_messages{op="redeliver"}`.
  - Delayed messages (rdb extension, Batch 2): `XADD <stream> [<id>] DELAY <ms>
    <field> <value>...` stages the body in a due-ordered kind-0x1D row invisible to
    every read path (XLEN keeps excluding it) until the due sweep
    (`lite.delay_sweep_ms`, default 0 = the scanner is not spawned at all) exchanges
    it into the stream in one latched WAL batch. The exchange appends a FRESH id — the
    XADD reply id is a reservation token only (a locked id below a group's delivered
    watermark would be invisible to `>` readers forever) — and wakes parked BLOCK
    readers (XADD-parity notify). Staged rows fold into family deletes and ride RENAME
    with the stream family; a purged/renamed-away stream is never revived by its
    leftovers (orphaned rows drop, never exchange).
  - XTRIM accepts `MINID [<~|=>] <id> [LIMIT <n>]` (Redis-aligned, orthogonal to
    MAXLEN): drops entries strictly below `<id>` (boundary survives); `~`/`=` behave
    identically (exact victims), LIMIT after both; `<ms>-0` ids = time-window retention.
    LIMIT-semantics ledger (P3 #9, resolved-as-covered): the `LIMIT <n>` clause means
    exactly the XTRIM parameter — a per-call budget on victims this round, budgeted
    BEFORE victims are chosen (LIMIT 0 = nothing trimmed); XADD-carried trims share the
    same parser and executor (`src/lite/append_opts.rs` trim_at/trim_victims), so no
    private LIMIT syntax exists. Pinned by `tests/lite_trim_minid_e2e.rs` (XTRIM
    LIMIT segmentation/LIMIT 0) and `tests/lite_xinfo_full_e2e.rs::
    xadd_trim_pins_xtrim_limit_semantics` (XADD `MINID ... LIMIT`).
  - XADD options (P3 backfill, 2026-10-07; parsing centralized in
    `src/lite/append_opts.rs`, shared with XTRIM): `NOMKSTREAM` (a missing stream does
    not create the key, replies nil — RESP2 `$-1`), `MAXLEN [~|=] <n>` /
    `MINID [~|=] <id> [LIMIT <n>]` trim in the SAME latched fsync batch as the append
    (the new entry itself is a victim when the plan reaches it, e.g. MAXLEN 0); options
    may precede or follow the id (once an option block precedes it the id is required),
    repeats override; XADD-carried trims pass the kind-0x20 ledger guard like XTRIM.
  - `XINFO STREAM <s> FULL [COUNT <n>]` (P3 backfill; Redis 7 shape, encoded in
    `src/lite/xinfo_full.rs`): stream level = length, last-generated-id, entries
    (newest `n`, id-ascending), groups; group level = name, last-delivered-id, pending
    rows `[id, consumer, ms-since-delivery, delivery-count]`, consumers; consumer
    level = name, seen-time (approximation: latest PEL delivery time, else registry
    time), pending (exact, never truncated), pel rows. Omitted-by-design fields (no
    engine data, never faked): radix-tree-keys/radix-tree-nodes, entries-added,
    max-deleted-entry-id, recorded-first-entry-id, group entries-read/lag. The non-FULL
    reply is unchanged.
  - Idle consumer GC (`lite.consumer_gc_ms`, default 0 = no background task at all,
    zero upgrade-visible change): a 1s-pace rotating reclaim of dead group members
    collectable only when ALL THREE hold — no PEL rows, no active lease (not parked in
    a waiting XREADGROUP, not the leased owner of an ordered queue), and idle past the
    threshold on the `seen_ms` registry clock (stamped by writes that already sync:
    delivery, XCLAIM/XAUTOCLAIM, XACK of owned rows; legacy rows fall back to
    created_ms). Removal reuses the XGROUP DELCONSUMER path in one synced batch
    (kill -9 durable — a collected member never reappears), at most 32 groups per
    round; an ordered owner's exemption is lease-scoped, not permanent.
  - XTRIM/XDEL ledger guard: a stream with ANY kind-0x20 committed-offset ledger row
    rejects both commands with `ERR stream <name> has committed consumer-group
    offsets; delete the groups first`; RENAME moves the rows with the stream family,
    old-name OffsetCommit answers error 3 UNKNOWN_TOPIC_OR_PARTITION.
  - Explicit-id XREADGROUP (any id other than `>`) reads only that consumer's own PEL
    history from disk; consumer idle times derive from the PEL's delivered_ms (no
    per-activity tracking).
  - XIDLE sets a per-stream idle TTL reusing the uniform expire envelope; expiry reaps the
    whole stream (entries + group state).
  - XPICK and XINFO TOPICS / XINFO LITE are rdb extensions; a bare parent name in XADD
    auto-picks a queue.
  - Physical slot prefix is derived from the PARENT topic name (CRC16), so all queues of a
    topic family co-locate and any node serves the family (all Lite verbs route-local).
  - Kafka wire-protocol compatibility was evaluated and rejected in the first pass (every
    current MQ gap is engine-level, and half-compat is a trust trap — Kafka clients expect
    acks=all/ISR/idempotence/transactions over a data plane that is node-local and
    non-replicated, plain KV included). Decision reversed 2026-09: the front landed in
    stages as a protocol adapter mapping parent/child to topic/partition exactly the way
    a sql/front does for MySQL — see the next bullet. Decision record and living Lite-MQ
    spec: [features/mq-lite.md](../features/mq-lite.md).
- **Kafka wire frontend (`kafka_bind`, rdb extension)**: 20 wire APIs over the same Lite
  engine (22 with SASL) — topic=parent stream, partition=child `p<N>`/`q<N>` queue,
  offset=ACTIVE-entry
  ordinal (not a physical offset), committed offsets in a separate kind-0x20 ledger.
  Single broker: Metadata always returns one node; acks=all = one synchronous fsync (no
  ISR/replicas). Compression is rejected by default (error 76); the optional
  `kafka-codecs` feature enables produce-side gzip/snappy/lz4 (fetch is never compressed;
  zstd unsupported). The group coordinator is in-memory (restart = clients rejoin,
  committed offsets persist); assignment comes from the consumer leader (standard broker
  behavior). Admin APIs (Batch 2): ListGroups(16) answers the union of coordinator
  runtime and ledger-only groups (an OffsetCommit-only group exists as kind-0x20 rows
  alone, reported state "Empty"); DeleteGroups(42) runs a group through the same public
  lite teardown `XGROUP DESTROY` uses (folding its 0x20 ledger rows — the wire-side
  release for the XTRIM/XDEL ledger guard) and evicts runtime state; a group with
  neither runtime entry nor ledger rows answers 69 while the rest of the batch proceeds.
  Topic-admin APIs (P3 backfill, 2026-10-07; classic framing only, all caps at the
  flexible boundary): CreateTopics(19) v0-v4 / DeleteTopics(20) v0-v3 /
  CreatePartitions(37) v0-v1 create the partition streams `T/p<N>` via one latched
  batch (`src/kafka/topic_store.rs`); validation ladder = name, assignments
  (any → 39 INVALID_REPLICA_ASSIGNMENT), replication_factor (only 1 or the -1 unset
  default; else 38 INVALID_REPLICATION_FACTOR — single node), partitions (-1 = broker
  default 1, else 1..=10000 or 37 INVALID_PARTITIONS), existence (36
  TOPIC_ALREADY_EXISTS / 3 UNKNOWN_TOPIC_OR_PARTITION); CreatePartitions takes the NEW
  total and a shrink answers 37; DeleteTopics folds the whole family per partition
  stream through the DEL family-delete path (entries+meta, kind-0x20 ledger, kind-0x1D
  delay rows, nested DLQ streams), request configs parsed and ignored.
  DescribeConfigs(32) v0-v3 is a stub: TOPIC resources answer a static minimal
  Kafka-default set (cleanup.policy=delete, retention.ms=604800000 — a documented
  constant, Lite streams carry no live retention, retention.bytes=-1,
  min.insync.replicas=1; config_source=DEFAULT), other resource types 42 INVALID_REQUEST;
  no AlterConfigs. OffsetForLeaderEpoch(23) v0-v3 is a constant answer: error 0,
  leader_epoch -1 (unknown — this broker reports no epochs anywhere; KIP-320 clients
  skip truncation), end_offset = the log end Fetch's high watermark uses. ListOffsets(2)
  is v0-v5 (was v0-v1): v2 adds parsed-and-ignored isolation_level + throttle field,
  v4+ decodes current_leader_epoch and answers leader_epoch -1; -3 max_timestamp
  answers latest (arrival ids keep no per-record max). `kafka_auto_create_topics`
  (default false = produce to an unknown topic still answers error 3, zero behavior
  change; true = a default single partition is created on first produce).
  Optional SASL PLAIN via `kafka_token` (empty = off, the default; when set, ApiVersions
  additionally advertises 17/36): SaslHandshake/SaslAuthenticate once per connection,
  constant-time password compare, wrong password = fixed-message 58 + close (no token
  fragments); pre-auth traffic is dropped without a reply except the ApiVersions +
  SASL-pair whitelist. `kafka_advertised_host/port` override the advertised listener (wildcard
  binds would otherwise advertise localhost); `kafka_max_connections` caps front
  connections (0 = 4096). Fetch replays REAL record headers for produce-shaped stored
  pairs (one "h" pair with valid headers JSON — header NAMES hex-encoded under the `"x"`
  key so arbitrary wire bytes round-trip byte-exact, legacy `"n"` string-name entries
  still readable; others at most one "k"/"v"/`__null__`); exotic shapes fall back to a
  JSON envelope value `{"fields":[[hex(name),hex(value)]]}` + marker header
  `("rdb-envelope", null)` — a genuine user header with that literal name wins and is
  replayed verbatim, never marked (storage forward-compatible, legacy data readable);
  ledger rows refuse XTRIM/XDEL and follow RENAME (error above). Spec:
  [features/kafka-front.md](../features/kafka-front.md).
- **JSON (P3, json.* verbs)**: single-record storage — one kind-0x10 record per key holds the
  whole document (LEB128 expire envelope + compact serde_json body, `preserve_order` keeps
  object key insertion order like Redis). Every mutation deserializes, mutates and re-serializes
  the full document; there is no sub-document addressing at the storage layer.
  - Only the legacy RedisJSON v1 deterministic path grammar is supported: root `.` or `$`,
    `.field`, `['field']`, `[index]` (composable, e.g. `.a[0].b` or `['odd.key'][2]`). Wildcards
    (`$..`, `[*]`), filters and recursive descent are rejected as `ERR wrong static path`.
    Legacy paths address exactly one node: reads/mutations on a missing path are `nil`/`0`,
    never "no match in multi-match" semantics.
  - `JSON.SET` on a missing key with a non-root path fails like RedisJSON v1 (there is no
    document to descend into); intermediate object fields are auto-created, but descending
    through a scalar is `ERR wrong type of path value`. `JSON.SET` at a non-existing *path*
    inside an existing doc reports `ERR path <path> does not exist` (path embedded, matching
    RedisJSON v1).
  - `JSON.GET` with multiple paths returns a flat RESP array of per-path serializations
    (Redis wraps them in a single synthetic object with legacy paths).
  - `JSON.ARRPOP` with an out-of-range index errors (`ERR index out of range`) instead of
    Redis' silent nil; `-1` pops the last element.
  - `JSON.NUMINCRBY` re-serializes numbers with serde_json's shortest-roundtrip formatting
    (e.g. `3.5`, `1e20` for overflow magnitudes); integral results below 2^53 are stored as
    i64, larger or fractional ones as f64. `JSON.TYPE` reports `integer`/`number` accordingly.
  - `JSON.MGET` aborts the whole command with WRONGTYPE if any key holds a foreign kind
    (Redis skips such keys).
  - `JSON.DEL`/`JSON.FORGET` are aliases; a root path drops the kind-0x10 record through the
    shared expire machinery (TTL index maintained), a sub-path splices the document.
- **VectorSet (P4, vadd/vrem/vcard/vdim/vsetattr/vgetattr/vsim)**: brute-force O(n*dim) cosine
  scan per VSIM (no HNSW graph, no EF/QSIP quantization) -- `FILTER`/`EF`/`EXPLORE` options are
  unimplemented and rejected as arity/unknown-option errors.
  - Vectors are stored raw f64 (kind-0x11 meta + kind-0x12 elem records, LE components; no L2
    normalization at rest -- cosine scoring makes it equivalent).
  - `score = (cos + 1) / 2` in [0,1]; a zero vector (either side) has cosine 0, i.e. score 0.5.
    Scores format as Rust's shortest-roundtrip f64 (`1`, `0.5`, `0.8535533905932737`), not
    Redis' fixed decimals.
  - `VDIM`/`VSIM` on a missing key error with `ERR vector set does not exist` (VSIM answers nil
    in Redis); `VSETATTR` on a missing key/element replies `:0`.
  - `VGETATTR` implements the single-attribute model only (Redis 8.2 adds multi-attribute
    `ATTRS`); the empty string clears back to the null bulk.
  - `VADD` on an existing element replaces the vector but KEEPS the stored attribute (Redis
    parity) and preserves the key's TTL; dimension must be 1..=4096 (`ERR invalid dim`) and
    match the set's (`ERR dimension mismatch`).
  - VSIM ties break by element byte order ascending (Redis breaks by internal HNSW order);
    `COUNT`/`WITHSCORES`/`WITHATTRIBS` parse in any order, `VALUES` swallows the argument tail.
- **RESP input hardening / connection hygiene** (the Go archive had none of these): a single
  `$N` bulk payload is capped at 512MiB (Redis `proto-max-bulk-len` parity; the header alone
  errors with `ERR Protocol error: invalid bulk length`, connection closed); a `*N` multibulk
  header preallocates at most 16 argument slots regardless of `N` (an eager giant `Vec` per
  parse retry was a dribble-amplified allocation DoS); the cumulative per-connection read
  buffer is capped at 1GB for AUTHENTICATED connections — an unauthenticated one is cut off
  at 64KB (`ERR Protocol error: too big cumulative request`, closed) and its 30s deadline is
  CUMULATIVE since first byte, so byte-dribbling cannot outlive it
  (`ERR unauthenticated connection timeout`, closed — once authenticated reads are unbounded);
  pipelined reply buffering is flushed to the socket at 64KB thresholds instead of growing
  until the pipeline drains; and a handler panic, after its unchanged `fatal error: <panic>`
  reply, now CLOSES the connection instead of leaving a possibly-desynced one open; the AUTH
  token is compared in constant time.
- **Ops-plane hardening** (none of these existed in the Go archive):
  - startup REFUSES an empty `raft_token` (`exit(1)` before binding) — an accidental no-auth
    cluster is a deployment error, not a supported mode (all `config/` files carry a token);
  - the `backup_target_map` seed loop RETRIES on raft apply failure (next 1s tick, idempotent
    overwrite + sentinel applied last) instead of Go's `log.Fatal`;
  - the join-on-startup decision uses openraft `is_initialized()` (persisted vote/log) — the
    Go code's "store dir exists" check and the naive "RocksDB CURRENT exists" check both
    false-positive after a FAILED first join (the dir/DB is created before the join RPC),
    which made the retry skip joining forever;
  - the raft apply channel is BOUNDED (1024); an overflowing control-plane write fails fast
    with `ERR: apply queue full` instead of queueing without limit;
  - SIGTERM/SIGINT trigger a graceful shutdown: log line, one bounded (5s) flush of the Lite
    group-offset watermarks, then `exit(0)` (Go had no signal handling; `kill -9` semantics
    for the data plane are unchanged — RocksDB WAL is the durability boundary);
  - blocking commands (BLPOP/BZPOPMIN/XREAD BLOCK) park on a dedicated bounded thread pool
    (`src/park.rs`), isolated from tokio's shared blocking pool where RocksDB fsyncs run —
    internal change, listed because 512+ concurrent blocking waits no longer stall writes
    (client-visible only as better tail latency).


### Transactions: MULTI/EXEC/DISCARD/WATCH/UNWATCH (per-request, optional)

Implemented at the application layer — NOT via RocksDB's
`OptimisticTransactionDB` (evaluated and rejected: base-store writes
conflict with open OCC transactions as engine-level `Resource busy`
failures on the hot path, transactional batches lack `delete_range` for
family deletes, and staged writes are invisible to command read paths —
no read-your-writes; see `src/store/rocksdb.rs::occ_engine_evaluation_record`).

Semantics:
- `MULTI` opens a queue on the connection; commands are validated at
  QUEUE time and replied `+QUEUED`. Queue-time rejections reply the
  error immediately AND mark the transaction dirty: unknown command,
  blocking commands (`BLPOP`/`BZPOPMIN`/`XREAD BLOCK`/...), `MIGRATE`
  and `RAFT` (cluster admin), arity/MOVED, nested `MULTI`, and
  `WATCH` inside `MULTI`.
- Single-slot rule: every key of every queued command must hash to the
  same slot (first keyed command binds it); violations reply
  `ERR CROSSSLOT Keys in request don't hash to the same slot` and mark
  the transaction dirty. Hash tags (`{u}a`, `{u}b`) co-locate as usual.
- `EXEC`: dirty transactions fail wholesale with
  `-EXECABORT Transaction discarded because of previous errors.`;
  otherwise the replies of all queued commands are returned as one
  array (`*N`), with per-command errors embedded in it (replay
  continues past single-command errors, Redis-style).
- Isolation: EXEC acquires the per-key latches of every queued key
  (byte-sorted, held across the whole replay) — handlers' own latch
  acquisitions are reentrant no-ops during replay, and all writes land
  while the latches are held. Watched keys are re-hashed under the
  latches; any change (including lazy expires performed by other
  connections' reads) aborts with a null array `*-1`.
- `WATCH` hashes the key's FULL physical family (raw string + every
  typed-kind range), so any layout change is detected. Any write
  executed on the connection outside MULTI implicitly UNWATCHes
  (including DEL of a missing key; a no-op cannot change any hash).
- `DISCARD` drops the queue and unwatches; `EXEC` always unwatches.
- Connection loss drops the transaction with it (state is per-connection).
- Config: `[tx] enabled` (default `true`); `false` makes `MULTI` reply
  `ERR transactions are disabled`.
- Metrics: `rdb_tx_events{event=queued|commits|aborts|conflicts}` and
  `rdb_tx_commit_latency` histogram.

Deviations vs Redis: commands unknown to this server are rejected at
queue time (Redis also does); `AUTH` is not queueable (this fork's AUTH
is a pre-dispatch connection gate); arity is not checked for some
commands until replay.


## Command-surface expansion (Rust-only, 2026-09-05)

The Go tree implemented only a small RESP subset. This round closes the Redis
command surface to 188 registered names; the following families exist only in
Rust (Go clients must treat them as new):

- **String**: INCR/DECR/INCRBY/DECRBY/INCRBYFLOAT, APPEND/STRLEN/GETSET/SETNX/
  SETEX/PSETEX/GETDEL/SETRANGE/GETRANGE. Semantics notes: arithmetic preserves
  TTL; GETSET clears it; SETRANGE that empties the value deletes the key;
  INCRBYFLOAT replies shortest-roundtrip f64 (`3`, not `3.0`); SETNX's NX veto
  fires for ANY existing key kind before any type check (`:0` on a hash key,
  Redis parity) while reads error WRONGTYPE.
- **Bits**: SETBIT/GETBIT/BITCOUNT/BITPOS/BITOP — MSB-first bit numbering
  within each byte, validated byte-for-byte against live Redis 7.2.5.
- **Server/meta**: COMMAND (bare/COUNT/INFO/DOCS/GETKEYS backed by a static
  188-row arity/first/last/step table kept in sync by test), INFO (Server/
  Cluster/Keyspace sections; Keyspace from the TTL-envelope index), DBSIZE
  (estimate-num-keys), ECHO, SELECT 0-only, FLUSHDB (chunked delete that
  preserves control-plane raft records; drops the in-process lite offset
  cache FIRST so the 200ms flusher cannot resurrect orphan consumer-group
  records — stale groups therefore read NOGROUP, Redis parity. FLUSHDB also
  serializes with in-flight lite offset flush rounds via the stream latch
  set, and double-clears the lite offset cache — before and after the chunked
  wipe — so no orphan group records survive on the wiped keyspace;
  stream-family deletion paths — XIDLE active-expire reap, DEL/EXPIRE of
  stream keys, lazy idle purge on read — now invalidate cached group offsets
  and queue a latched orphan sweep guarded against streams recreated between
  family delete and sweep).
- **Gap fills**: HMSET, ZREVRANGE, SINTERCARD, LMPOP, XREVRANGE.
- **Routing**: LMPOP/SINTERCARD/BITOP derive their routing slot from argv[2]
  (the first real key), not argv[1] — the numkeys token / operation word is
  never hashed (`router::routing_key_index`). MULTI queue-time slot checks use
  the same table.

## Full-text + vector search (FT.*, Rust-only)

The Go archive has no search engine; the Rust tree ships one on the typed-record
data plane (kinds `0x13`-`0x18`, one physical family, so index-key TTL purges
docs, postings, term stats, centroids and ANN partitions together).

- Commands: `FT.CREATE <idx> SCHEMA <f> TEXT | <f> VECTOR DIM <n>` (at most one
  VECTOR field), `FT.ADD <idx> <docid> <json>` (add/replace, one fsync),
  `FT.DEL <idx> <docid>`, `FT.DROP`/`FT.dropindex`, `FT.INFO`, `FT.BUILD [K n]
  [ITERS n] [SEED n]`, `FT.SEARCH <idx> <query> [LIMIT o c] [WITHSCORES]
  [NOCONTENT] [NPROBE n] [KNN k <field> FP16 <blob>|VALUES v...]`.
- Tokenizer (index and query sides are identical by construction): Latin
  alphanumeric runs lowercased; Han runs cut by jieba (embedded dict + HMM),
  dictionary misses fall back to overlapping bigrams (the CJKAnalyzer trick);
  an isolated single character emits itself.
- Text ranking: BM25 (Lucene idf `ln(1+(n-df+0.5)/(df+0.5))`, K1=1.2, B=0.75),
  terms AND-ed within a query, ties broken by docid ascending.
- Vectors: SQ8 quantization (per-dimension min/scale, field-global calibration)
  plus k-means centroids in a SPANN-style partition layout. `FT.BUILD` retrains
  and repartitions; KNN probes `nprobe` nearest partitions and reranks on
  dequantized vectors, score = `1/(1+L2)`. Without a trained table KNN
  degrades to exact brute force. A `@field:term` query before `KNN`
  prefilters candidates by text match (exact vectors, no SQ8 error).
- Cross-node: an index lives on the slot of its key; non-local requests get
  `MOVED` like any other command. Cross-index queries are client fan-out —
  there is no distributed query planner. `FT.SEARCH` replies are node-local:
  `:N` then per hit docid [, score] [, original JSON].

## SQL data plane (MySQL wire, Rust-only)

The Rust tree adds a SQL data plane the Go implementation never had. The contract
(access/auth, MVCC rows, timestamps, transactions, locking reads, indexes,
AUTO_INCREMENT, temporal/decimal/composite-pk typing, planner, query language &
expressions/functions, DML conflict paths, DDL & session surface, 2PC writes,
scatter-gather reads, GC, plus the MySQL-gap intentional-deviation ledger) outgrew
this file's size budget at the 2026-10-06 MySQL-gap M0-M5 sync and lives in
[COMPAT.sql.md](./COMPAT.sql.md). Module map: `agents/rust/sql.md`; feature summary:
`features/sql-dataplane.md`.

## Columnar table engine (Rust-only)

A second storage engine behind the same SQL front end: `CREATE TABLE ... ENGINE=columnar`
(the default stays the MVCC row store). Append-only and scan-oriented; segments are
immutable once published.

- **DDL**: `ENGINE=columnar` table option (case-insensitive). Column types are the row
  store's (`BIGINT`, `VARCHAR(n)`, `DOUBLE`, `BOOLEAN`/`BOOL`, `BLOB`); CREATE/DROP run
  through the same raft-linearized catalog as row tables.
- **Physical layout**: segment files live under `<store_path>/<bind>/columnar/`
  (magic | per-column pages | JSON footer | footer_len | crc32; PLAIN and string DICT
  encodings with per-page zonemaps) — never inside RocksDB, whose LSM compaction would
  rewrite them. RocksDB stores only the small `0x23 | table_id BE | segment_id BE` meta
  per segment (state, commit_ts, file name, column layout); a per-instance in-memory
  registry caches the live metas.
- **Writes**: append-only. Autocommit INSERT flushes one Live segment per statement and
  commits it locally on a single timestamp (never 2PC); inside an explicit txn rows are
  staged and COMMIT flushes one segment per table into the txn's atomic batch (in a ready
  cluster the segment rides the 2PC plan as a pending segment on the coordinator's own
  slice). UPDATE / DELETE / CREATE INDEX on a columnar table are rejected (MySQL 1235).
  No pk dedup, no secondary indexes. Per-statement and staged buffers are bounded by
  `columnar_flush_rows` / `columnar_flush_bytes`.
- **Reads**: visibility is segment-level (`commit_ts <= read_ts`); scans decode Live
  segments in `(commit_ts, segment_id)` order plus an open txn's staged appends. In a
  ready cluster every SELECT fans out to EVERY member (EXPLAIN shows
  `Gather(columnar, nodes=N)`) and the coordinator merges the union — segments live where
  the committing INSERT ran. A member's snapshot read point is its own ts knowledge, so
  right after concurrent INSERTs on different nodes a reader may need its read point
  advanced (any 2PC row-store commit does that via Decide) before it sees every segment.
- **DROP TABLE**: after the raft catalog tombstone lands, the DDL-executing node deletes
  every `0x23` meta of the table in one contiguous scan + one batch, forgets the registry
  entries and best-effort unlinks the files. Cluster-wide this mirrors the row store: only
  the executing (leader) node purges, segments on other members become unreachable orphans
  behind the tombstone, reclaimed by the M5 GC sweep (`sql::columnar::gc`: 30s rounds;
  unreferenced files are only unlinked past a 1h age floor; an empty raft catalog view
  keeps every segment meta for the round). The row store mirrors this exactly: only the
  executing (leader) node purges eagerly, and every node's MVCC GC sweep (above)
  reclaims the orphaned row versions and index entries of dropped tables within ~30s.
- **Slot migration**: segments are files outside the slot-keyed keyspace and do NOT move
  with their slot band: after a reshard, pre-existing segments stay on the node that
  committed them (reads fan out to every member, so they remain visible); new segments
  are placed by current slot ownership, and a node owning no slot band rejects columnar
  INSERTs ("no eligible slot band").

## StarRocks table-model DDL (Rust-only)

StarRocks-compatible `CREATE TABLE` headers are accepted and mapped onto the two existing
storage engines. No new engine: the mapping is syntax-level; placement stays the CRC16
slot sharding (see deviations).

- **Accepted headers**:
  `PRIMARY KEY(col[, col...]) [DISTRIBUTED BY HASH(col) [BUCKETS n]]` -> row store,
  INSERT-as-UPSERT (single- or multi-column pk, engine composite-pk rules apply).
  `DUPLICATE KEY(cols...) [DISTRIBUTED BY HASH(col) [BUCKETS n]]` -> columnar, append-only.
  Distribution is optional (`BUCKETS` defaults to StarRocks' 10). A MySQL-level pk may be
  written inline (`col INT PRIMARY KEY`) or as a constraint; for DUPLICATE tables a MySQL
  `PRIMARY KEY` is not required — the first DUPLICATE KEY column becomes schema-only pk
  metadata, keeping its declared nullability.
- **Pre-parser**: sqlparser has no StarRocks grammar, so the model clauses are lifted from
  raw text BEFORE the MySQL parse (token scan over comments/quoted spans in
  `sql::parse::starrocks`). Unrecognized clauses — `PARTITION BY`, `PROPERTIES`,
  `ORDER BY`, `UNIQUE KEY(...)`, `DISTRIBUTED BY RANDOM`, `ENGINE=row`/`innodb` on a
  model table, `PRIMARY KEY` + `ENGINE=columnar`, and `AGGREGATE KEY` — reject loudly
  with MySQL 1235 instead of being dropped silently. The PK model's key list is injected
  back as a MySQL `PRIMARY KEY(...)` constraint, so multi-column `PRIMARY KEY(col1,col2)`
  flows into the composite-pk path (same column-type narrowing; see the SQL data plane
  contract in [COMPAT.sql.md](./COMPAT.sql.md)) — the Phase-4 single-pk restriction is gone.
- **Schema metadata**: `TableSchema.key_model` (`MySql` | `PrimaryKey` | `Duplicate`) and
  `TableSchema.distribution` (`{columns, buckets}`) persist in the raft catalog JSON,
  both `#[serde(default)]`: old catalog JSON decodes as `MySql`/none. Because old nodes
  cannot render the new fields, ROLLING upgrades are NOT safe once a StarRocks DDL exists:
  co-upgrade nodes together (same gate as any catalog-shape change).
- **PK-model writes**: autocommit INSERT recovers each pk's visible row BEFORE stamping
  (statement read point) and feeds index maintenance a replace, so unique entries move
  with the values; within explicit txns, staging already keyed rows by `(table, pk)`, so
  last-write-wins collapse is inherent and COMMIT derives replaces from the snapshot.
  UPDATE / DELETE behave as usual on the row store.
- **Deviations from real StarRocks** (by design):
  - DISTRIBUTED BY is recorded as schema metadata only: physical placement remains the
    Redis-style crc16 slot sharding; no buckets/buckets rebalance exist. `BUCKETS n`
    validates `n >= 1` and nothing else.
  - Cluster mode: a PK-model upsert coordinated by a node that does not own all target
    slots ships only the NEW versions through 2PC (old-side recovery is coordinator-local).
    Row correctness holds everywhere (one version per pk wins by ts); whether the new value
    shadows older seeds on every reader follows cluster-wide timestamp ordering — the same
    M2-era caveat ordinary cross-node UPDATEs already carry, so secondaries/unique entries
    plus cross-coordinator replace recency stay follow-up work (composite keys themselves
    landed in the W2 batch; multi-column tuples ride the same caveat, not a new one).

## Elasticsearch-compatible frontend (Rust-only)

An ES-style HTTP/JSON frontend (`es_bind`, optional `es_token` Bearer auth) exposes the SAME
search kernel as the FT.* commands (see "Full-text + vector search" above). It is a protocol
adapter, not an ES clone; the deviations below are the contract.

- **Indexing model**: one index = one user key = one slot — the index NAME is the storage key,
  so hash-tag it (`{books}`) to colocate an index with its docs on one node. No shards,
  replicas, settings, aliases or index templates exist. A request naming a slot this node does
  not own is NOT proxied or redirected: it fails with HTTP 400 `routing_exception` naming the
  owning node (the HTTP analogue of RESP's MOVED; cross-cluster access is client fan-out).
- **Concurrency/versioning**: `_version` is always 1 and `_seq_no`/`_primary_term` always
  0/1 — there is no optimistic concurrency control (`?if_seq_no` etc. ignored; `op_type=create`
  on an existing id still 409s).
- **Refresh semantics**: `_refresh` is a no-op returning success (reads are realtime); the
  `?refresh` query parameter is ignored.
- **`match` operator default is `or`**: multi-term `match` unions the per-term postings and
  sums BM25, whereas FT.SEARCH query strings AND their terms. `match` with an empty or
  whitespace-only query matches nothing.
- **Term semantics**: keyword `term`/`terms` compare exact bytes (case-sensitive, no
  analyzer — `Redis` != `redis`). Numeric fields accept exactly one scalar number (arrays are
  rejected with 400). `term`/`range` on an unknown or non-numeric field match nothing (0 hits)
  instead of erroring.
- **knn**: exact L2 distance over the filtered candidate set, SPANN probe otherwise; score is
  `1/(1+L2)`. `num_candidates` maps to the SPANN `nprobe = clamp(n/16, 1, 64)`. At most one
  VECTOR field per index.
- **Not implemented**: aggs, highlight, scroll, script_score, `minimum_should_match`, custom
  analyzers, index templates, aliases. An unsupported `_bulk` `update` action yields a
  per-item error inside the 200 `_bulk` envelope.
- **sort by field** reads `_source` per hit (no doc-values); documents missing the sort field
  sort last; `from + size` is capped at 10000 (400 beyond).
- **Transport**: HTTP/1.1 only, one request per connection (`Connection: close`, no
  keep-alive), Content-Length bodies only (`Transfer-Encoding: chunked` -> 501). Optional
  `Authorization: Bearer <es_token>` when `es_token` is non-empty.

## RocksMQ-compatible HTTP frontend (Rust-only)

A minimal RocksMQ-style HTTP/1.1 message frontend (`rocksmq_bind`, reuses the hand-rolled
HTTP of the ES front) over the same Lite engine as the Kafka front — four POST endpoints:
`/produce` (XADD; optional `delay_ms` query stages a delayed message, replies the Lite
`<ms>-<seq>` id — with `delay_ms>0` that id is the XADD-time reservation token, the due
exchange appends a fresh id), `/consume` (grouped XREADGROUP `>`; group-less tail pull
via XREVRANGE keeps no cursor — duplicates/skips are documented; optional `wait_ms` parks
the request until a message lands or the budget expires — long-poll semantics), `/ack`
(XACK, idempotent 200; unknown group 404), `/pending` (XPENDING summary shape for one
channel/group). Optional `rocksmq_token` (empty = off, the default) requires
`Authorization: Bearer <token>` on every route (401 otherwise). A bare channel name maps
to `NAME/q0`; the body is the `v` field pair, byte-identical with the Kafka front's
keyless/headerless records, so the two fronts read each other's messages. Batch + replay
routes (P3 backfill, 2026-10-07): `POST /produce_batch` and `POST /ack_batch` take a
JSON array (max 100 elements, the same `MAX_BATCH` as `/consume`'s `n`) and re-dispatch
each element through the single-route handler (per-item `status`+`body` results in
request order, one bad item never aborts the rest; batch-level 400 only for non-JSON /
non-array / over-cap bodies); `GET /range?channel[&begin][&end][&limit]` is a read-only
XRANGE replay (no PEL registration, no group state, invisible-to-delayed rows like every
read path; unknown channel → `{"msgs":[]}`); all three pass the bearer gate.
`rocksmq_max_connections` (default 0 = built-in 4096, negative falls back to it) caps
concurrent front connections — at cap a new TCP connection is closed silently (no HTTP
bytes written), mirroring the kafka front's guard. Remaining
deviations vs real RocksMQ (`<ms>-<seq>` ids not integer offsets, no topic create/delete/
seek): [features/rocksmq-http.md](../features/rocksmq-http.md).

## S3-compatible object-storage frontend (Rust-only)

An S3-protocol frontend (`s3_bind`, optional `s3_token` Bearer auth — NOT SigV4) serves
objects from the local filesystem (`s3_store_path`, empty falls back to `<store_path>/s3`):
one object = one real file plus a `.s3meta.json` sidecar, written atomically (tmp + fsync +
rename). Buckets, ListObjectsV2 (prefix/delimiter/common-prefixes, max-keys capped at 1000,
continuation-token or v1 marker, encoding-type=url), single-range GET (206/416), always-204
DELETE, one request per connection (chunked -> 501, 1 GiB body cap); no multipart, no
versioning, directory-marker keys (trailing `/`) fail with InvalidArgument. RocksDB
checkpoints are periodically published StarRocks-tablet style under
`<bucket>/rocksdb/<node-bind>/ckpt_<unix_ms>/` (file set + a final `meta.json`, the rowset-
meta analogue), retaining the newest `s3_checkpoint_retention` (default 2) so a backup
instance can pull a self-consistent DB directory with any S3 client. The Go archive has no
such frontend. Spec: [features/s3-object-storage.md](../features/s3-object-storage.md).

## Runtime verification (this tree)

- Full RESP drill (gate text, cluster init/nodes, MOVED format+routing, hash-tag co-location,
  set/get/del, raft set/get cross-node, follower write error, unknown command, NOAUTH): all pass.
- Soak: 937 writes / 145s and 742 writes bursts — 0 slow (>1s), 0 errors, 0 beacon gaps.
- HA: leader kill -9 → new leader in ~6s → writes commit → node restart rejoins as follower
  and catches up (verified both pre- and post-failover keys).
- `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test
  --workspace` green (995 tests).
