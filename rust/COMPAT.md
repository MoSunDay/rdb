# COMPAT.md — Rust `rdb` vs Go `rdb` compatibility notes

The Rust implementation is byte-compatible with the Go implementation on the **RESP data plane**
and the Raft **HTTP API**. The Raft **TCP wire protocol is intentionally different** (openraft
JSON framing vs hashicorp msgpack), so Go and Rust nodes cannot join the same raft cluster.

## Critical build requirement (tokio LIFO-slot freeze)

`rust/.cargo/config.toml` sets `rustflags = ["--cfg", "tokio_unstable"]` so that
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

If you build the binary without `rust/.cargo/config.toml` in scope (e.g. building from outside
the `rust/` directory), set `RUSTFLAGS='--cfg tokio_unstable'`. At startup the binary now
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

## Preserved Go quirks (byte-compatible)

- `MGET`/`MSET` route by the **first key only** (all keys go to the first key's node).
- `cluster test` returns the hardcoded literal `-MOVED 5465 127.0.0.1:32681`.
- Slot range routing uses inclusive upper bound: `slot <= (i+1)*per`, `per = 16384/len`.
- `migrate task` overwrites `migrate_task` unconditionally.
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
- **Lite Mode (RocketMQ-style, rdb extension)**: parent topics with dynamic per-group queues
  exposed through Streams-verb commands (XADD/XLEN/XRANGE/XTRIM/XDEL/XIDLE/XREAD/XREADGROUP/
  XACK/XGROUP/XINFO/XPICK). This is not a Redis Streams emulator:
  - XREAD/XREADGROUP accept exactly ONE stream (multi-stream syntax is an error).
  - No PEL: XACK persists the group's committed watermark synchronously (kind-0x0E record,
    delivered := committed), so a kill -9 restart resumes from the watermark and redelivers
    only post-watermark entries — at-least-once semantics, not exactly-once.
  - XIDLE sets a per-stream idle TTL reusing the uniform expire envelope; expiry reaps the
    whole stream (entries + group state).
  - XPICK and XINFO TOPICS / XINFO LITE are rdb extensions; a bare parent name in XADD
    auto-picks a queue.
  - Physical slot prefix is derived from the PARENT topic name (CRC16), so all queues of a
    topic family co-locate and any node serves the family (all Lite verbs route-local).
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
  errors with `ERR Protocol error: invalid bulk length`, connection closed); the cumulative
  per-connection read buffer is capped at 1GB (`ERR Protocol error: too big cumulative
  request`, closed); unauthenticated connections get a 30s read deadline
  (`ERR unauthenticated connection timeout`, closed — once authenticated reads are unbounded);
  and a handler panic, after its unchanged `fatal error: <panic>` reply, now CLOSES the
  connection instead of leaving a possibly-desynced one open; the AUTH token is compared in
  constant time.
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

## Runtime verification (this tree)

- Full RESP drill (gate text, cluster init/nodes, MOVED format+routing, hash-tag co-location,
  set/get/del, raft set/get cross-node, follower write error, unknown command, NOAUTH): all pass.
- Soak: 937 writes / 145s and 742 writes bursts — 0 slow (>1s), 0 errors, 0 beacon gaps.
- HA: leader kill -9 → new leader in ~6s → writes commit → node restart rejoins as follower
  and catches up (verified both pre- and post-failover keys).
- `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test
  --workspace` green (123 tests).
