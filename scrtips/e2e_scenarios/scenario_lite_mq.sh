#!/usr/bin/env bash
# scenario_lite_mq.sh - e2e: Lite MQ (RocketMQ-5.5-Lite semantics on the
# Redis Streams verbs) driven by REAL redis-cli against the 3-node rdb
# cluster assembled by env.sh: first CLI-suite coverage of what only the
# Rust tests pin (tests/lite_*_e2e.rs). Steps: (a) produce (bare-parent
# queue pick, explicit queue, XLEN, XRANGE COUNT), (b) XGROUP CREATE +
# XREADGROUP `>` COUNT handoff, (c) XPENDING summary/range + XACK,
# (d) second consumer + XAUTOCLAIM takeover, (e) ORDERED strict-serial
# group + INFLIGHT knob, (f) XINFO STREAM / XINFO GROUPS, (g) XGROUP
# MAXDELIVERY dead-lettering into the default <stream>/dlq + consuming
# the DLQ as a plain stream, (h) XTRIM MINID threshold trim (exact id,
# LIMIT cap, <ms>-0 time-window form), (i) delayed messages (XADD DELAY
# staging invisible before due, due exchange with a FRESH id, a parked
# BLOCK reader woken by the exchange, RENAME carrying the staged row),
# (j) P3 W2 backfill verbs: XADD NOMKSTREAM (nil on a missing key, no
# stream materialized), XINFO STREAM <key> FULL [COUNT n] deep view
# (entries + groups nesting), XCLAIM RETRYCOUNT replacing the PEL
# delivery counter (visible in XPENDING's deliveries column).
# Semantics verified against source/tests BEFORE writing this script:
# - Every X-command is cluster-whitelisted => NODE-LOCAL (src/router.rs:96
#   is_whitelisted; src/command/mod.rs:437 skips slot routing). The
#   physical prefix is the CRC16 slot of the PARENT name
#   (src/lite/model.rs:111-119), so all Lite traffic below targets ONE
#   node (the raft leader): another node owns a different local RocksDB.
# - Stream names must be `parent/child`. Every verb but XADD rejects a
#   bare parent with "ERR a full stream name 'parent/child' is required"
#   (src/lite/entries.rs:89-105); XADD alone auto-picks a queue (q0 for a
#   brand-new topic, src/lite/select.rs:32,80-90) and then replies the
#   2-element array [full-stream, id] (src/lite/append.rs:174-178).
# - ids are `<u64>-<u64>` (src/lite/model.rs:42-50): a bare "0" is NOT an
#   id, so XGROUP CREATE s g 0 answers "ERR Invalid stream ID specified
#   as stream command argument" (src/lite/group.rs:47,127-133) while
#   `0-0` and `$` are the Rust tests' start points; duplicate CREATE =>
#   BUSYGROUP (tests/lite_pel_e2e.rs:26-36).
# - XREADGROUP GROUP g c [COUNT n] STREAMS <parent/child> `>` delivers
#   NEW entries past the group watermark to the NAMED consumer and mints
#   PEL rows (src/lite/read.rs:614-620). A total miss is the nil array
#   `*-1`, which redis-cli --raw prints as ONE EMPTY LINE.
# - XPENDING <s> <g> summary = [total, min-id, max-id, consumer, count];
#   empty PEL = [0, nil, nil] (src/lite/pending.rs:58-83) => "0" plus two
#   empty lines. Range `XPENDING s g - + n` = [id, consumer, idle,
#   deliveries] rows in id order (tests/lite_pel_e2e.rs:96-116).
# - XACK replies the committed-watermark ADVANCE, not rows removed
#   (tests/lite_pel_e2e.rs "ack_below_watermark..."); a contiguous drain
#   replies the acked count (tests/lite_proc_e2e.rs:185-190).
# - XAUTOCLAIM <s> <g> <c> <min-idle> <start> [COUNT n] replies the
#   Redis>=7 shape [next-cursor, entries[], deleted-ids[]]: entries carry
#   FULL frames (id + field/value pairs), the cursor is the SUCCESSOR of
#   the last scanned row (0-0 when the scan ran off the PEL end), and a
#   60000ms idle gate claims nothing (src/lite/autoclaim.rs:164-213).
# - ORDERED is an rdb XGROUP CREATE option spelled `ORDERED [INFLIGHT n]`
#   (src/lite/group.rs:70,76-105); INFLIGHT without ORDERED is a syntax
#   error, n >= 1. Default ORDERED = inflight 1 = strict serial; the queue
#   has ONE owner (30s lease, src/lite/mod.rs:83-90) so a second consumer
#   gets the nil array, not a steal, and only the owner's XACK frees the
#   window (tests/lite_ordered_e2e.rs:34-60,86-98).
# - XINFO STREAM <s> = [length, last-generated-id, groups, idle-ms]
#   (src/lite/info.rs:30-48). XINFO GROUPS <s> per-group arrays carry 13
#   elements (name + 6 labeled fields). Fixed 2026-09-26: the header
#   used to declare *14 with only 13 following, which hung strict RESP
#   clients (redis-cli) forever -- the `timeout` wrapper below turns a
#   regression of that into a hard failure instead of a hang.
# - XADD accepts a leading `DELAY <ms>` option right after the id
#   (src/lite/append.rs -> lite::delay): the message is staged in a
#   kind-0x1D row that NO read path can see (XLEN keeps excluding it)
#   until the spawned due sweep exchanges it into the stream; the XADD
#   reply id is a RESERVATION token only -- the exchange appends a FRESH
#   id (a locked id below a delivered watermark would be lost forever,
#   src/lite/delay.rs ID POLICY) and wakes parked BLOCK readers
#   (XADD-parity notify on the stream + parent keys). The sweep exists
#   only when `lite.delay_sweep_ms` > 0 (default 0 = OFF); family
#   delete/RENAME fold/carry the staged rows (tests/lite_delay_e2e.rs).
# - P3 W2 (section j), verified against source before writing it:
#   NOMKSTREAM is a LEADING XADD option; on a FRESH key the nil bulk is
#   the whole reply -- no meta, no entry, no stats (src/lite/append.rs,
#   checked BEFORE id resolution) -- while an existing stream appends
#   normally. Lite streams are keyed under the PARENT topic's slot
#   (src/lite/model.rs::stream_prefix) so slot-routed EXISTS can never
#   see one (0 even after creation, or MOVED when the child slot is
#   another node's): the node-local absence oracle is the whitelisted
#   KEYS (empty <-> the exact key). XINFO STREAM <s> FULL =
#   [length, last-generated-id, entries[][], groups[][]] where each
#   group nests name/last-delivered-id/pending[]/consumers[]
#   (src/lite/xinfo_full.rs; entries and PEL lists cap at COUNT n,
#   default 10). XCLAIM ... RETRYCOUNT n REPLACES the PEL delivery
#   counter outright (a hint-less claim then bumps from it,
#   src/lite/claim.rs) -- XPENDING's 4th range column reads it.

# env.sh installs `set -uo pipefail` and the assertion helpers. Deliberately
# NO `set -e`: assertion failures count into E2E_FAILS so the scenario
# always runs to its e2e_finish summary.
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

# rdb gates every pre-AUTH command behind AUTH (env.sh exports this too).
export REDISCLI_AUTH="${RDB_E2E_TOKEN}"

# Batch 2 (i): arm the staged-delay due sweep. e2e_start_cluster has
# already generated every node yaml when it calls _e2e_spawn, so
# wrapping the spawner appends the knob to the deterministic yaml path
# just before first boot -- no env.sh edit, no restart dance (the
# declare -f copy below is how bash wraps a sourced function). Adding
# it globally is safe for segments (a)-(h): the sweep ONLY walks
# kind-0x1D staging rows and no other segment stages any (plain XADD
# never writes 0x1D), and production default is 0 = the scanner is not
# spawned at all (src/lite/delay.rs::spawn_delay_sweep).
eval "_e2e_spawn_plain () $(declare -f _e2e_spawn | tail -n +2)"
_e2e_spawn () {
    printf 'lite:\n  delay_sweep_ms: 200\n' \
        >>"$E2E_WORKDIR/conf_node$1.yaml"
    _e2e_spawn_plain "$@"
}

RC="redis-cli -h $E2E_HOST -p"

rc () { # one redis-cli call against node <idx>: idx args...
    local idx=$1
    shift
    $RC "$(node_resp "$idx")" --raw "$@" 2>/dev/null
}

nth () { printf '%s\n' "$2" | sed -n "$1p"; } # 1-based line of a reply

count_line () { # how many reply lines equal $1 exactly
    printf '%s\n' "$2" | grep -cxF "$1" || true
}

# Poll `$2...` (a command) until its output contains $1, at most $3
# seconds (0.2s rhythm): the (i) segment's due-exchange waits. Bounded
# so a broken sweep surfaces as failed asserts below, never a hang.
until_grep () { # needle timeout_s cmd...
    local needle=$1 budget=$2 out=""
    local deadline=$((SECONDS + budget))
    while [ "$SECONDS" -lt "$deadline" ]; do
        out="$("${@:3}" 2>/dev/null)"
        [ -n "$out" ] && printf '%s\n' "$out" | grep -qF "$needle" && break
        sleep 0.2
    done
    printf '%s\n' "$out"
}

# RAW RESP one-shot probe: AUTH + one command over bash /dev/tcp, printing
# up to <max> reply lines (CR stripped, 3s per-line cap so a truncated
# frame just stops early). Needed only for XINFO GROUPS -- see header.
resp_raw () { # idx max args...
    local idx=$1 max=$2
    shift 2
    local port frame part t a n line
    port=$(node_resp "$idx")
    t=${#RDB_E2E_TOKEN}
    printf -v frame '*2\r\n$4\r\nAUTH\r\n$%d\r\n%s\r\n' "$t" "$RDB_E2E_TOKEN"
    printf -v part '*%d\r\n' "$#"
    frame+="$part"
    for a in "$@"; do
        printf -v part '$%d\r\n%s\r\n' "${#a}" "$a"
        frame+="$part"
    done
    exec 3<>"/dev/tcp/$E2E_HOST/$port" || return 1
    printf '%s' "$frame" >&3
    n=0
    while [ "$n" -lt "$max" ]; do
        IFS= read -r -t 3 line <&3 || break
        printf '%s\n' "${line%$'\r'}"
        n=$((n + 1))
    done
    exec 3<&- 3>&-
}

main () {
    local scenario=lite_mq
    e2e_init "$scenario"
    e2e_start_cluster >/dev/null || { echo "FATAL: cluster bring-up failed" >&2; exit 1; }
    local mq
    mq=$(e2e_find_leader) || { echo "FATAL: no leader" >&2; exit 1; }
    echo "lite node: n$mq (X-commands are node-local, so one node sees all)"

    # ---- (a) produce: queue pick, explicit queue, XLEN, XRANGE ---------
    local bare id0 id1 id2 id3 rng
    bare="$(rc "$mq" XADD orders '*' sku widget)"
    assert_eq "bare-parent XADD replies the auto-picked queue name" \
        "orders/q0" "$(nth 1 "$bare")"
    id0="$(nth 2 "$bare")"
    assert_contains "bare-parent XADD replies the generated id" "-" "$id0"
    id1="$(rc "$mq" XADD orders/q0 '*' sku s1)"
    id2="$(rc "$mq" XADD orders/q0 '*' sku s2)"
    id3="$(rc "$mq" XADD orders/q0 '*' sku s3)"
    assert_eq "XLEN counts the four appended entries" "4" \
        "$(rc "$mq" XLEN orders/q0)"
    assert_contains "a bare parent is not a stream name for XLEN" \
        "ERR a full stream name 'parent/child' is required" \
        "$(rc "$mq" XLEN orders)"
    rng="$(rc "$mq" XRANGE orders/q0 - + COUNT 2)"
    assert_eq "XRANGE COUNT caps the reply at 2 entries" "2" \
        "$(count_line sku "$rng")"
    assert_not_contains "XRANGE COUNT 2 hides the newest entry" "$id3" "$rng"

    # ---- (b) group + first delivery -------------------------------------
    assert_contains "XGROUP CREATE needs a <ms>-<seq> id, not a bare 0" \
        "ERR Invalid stream ID specified as stream command argument" \
        "$(rc "$mq" XGROUP CREATE orders/q0 mygroup 0)"
    assert_eq "XGROUP CREATE 0-0 MKSTREAM subscribes from the start" "OK" \
        "$(rc "$mq" XGROUP CREATE orders/q0 mygroup 0-0 MKSTREAM)"
    assert_contains "re-creating the group answers BUSYGROUP" "BUSYGROUP" \
        "$(rc "$mq" XGROUP CREATE orders/q0 mygroup 0-0 MKSTREAM)"
    local d1
    d1="$(rc "$mq" XREADGROUP GROUP mygroup c1 COUNT 2 STREAMS orders/q0 '>')"
    assert_contains "XREADGROUP names the child stream in the reply" \
        "orders/q0" "$d1"
    assert_eq "COUNT 2 hands exactly 2 entries to c1" "2" \
        "$(count_line sku "$d1")"
    assert_not_contains "c1 did not get the third entry" "$id2" "$d1"

    # ---- (c) PEL: summary, range rows, one ack --------------------------
    local p
    p="$(rc "$mq" XPENDING orders/q0 mygroup)"
    assert_eq "XPENDING summary total" "2" "$(nth 1 "$p")"
    assert_eq "XPENDING summary min id" "$id0" "$(nth 2 "$p")"
    assert_eq "XPENDING summary max id" "$id1" "$(nth 3 "$p")"
    assert_eq "XPENDING summary consumer" "c1" "$(nth 4 "$p")"
    p="$(rc "$mq" XPENDING orders/q0 mygroup - + 10)"
    assert_eq "XPENDING range row 1 id" "$id0" "$(nth 1 "$p")"
    assert_eq "XPENDING range row 1 consumer" "c1" "$(nth 2 "$p")"
    assert_eq "XPENDING range row 1 deliveries (first delivery)" "1" \
        "$(nth 4 "$p")"
    assert_eq "XPENDING range row 2 id" "$id1" "$(nth 5 "$p")"
    assert_eq "XACK of the first id advances the watermark by 1" "1" \
        "$(rc "$mq" XACK orders/q0 mygroup "$id0")"
    p="$(rc "$mq" XPENDING orders/q0 mygroup)"
    assert_eq "XPENDING shrank to one row" "1" "$(nth 1 "$p")"
    assert_eq "XPENDING min==max once one row is left" "$(nth 2 "$p")" \
        "$(nth 3 "$p")"

    # ---- (d) second consumer + XAUTOCLAIM takeover ----------------------
    local d2 ac
    d2="$(rc "$mq" XREADGROUP GROUP mygroup c2 STREAMS orders/q0 '>')"
    assert_contains "c2 drains the rest: newest entry" "$id3" "$d2"
    assert_not_contains "c2 cannot re-read c1's acked id" "$id0" "$d2"
    p="$(rc "$mq" XPENDING orders/q0 mygroup)"
    assert_eq "two consumers now hold pending rows" "3" "$(nth 1 "$p")"
    ac="$(rc "$mq" XAUTOCLAIM orders/q0 mygroup c3 0 0-0 COUNT 10)"
    assert_eq "XAUTOCLAIM cursor 0-0: the scan ran off the PEL end" "0-0" \
        "$(nth 1 "$ac")"
    assert_contains "XAUTOCLAIM returns full entry frames (oldest)" \
        "$id1" "$ac"
    assert_contains "XAUTOCLAIM returns full entry frames (newest)" \
        "$id3" "$ac"
    assert_eq "XAUTOCLAIM claimed every pending row" "3" \
        "$(count_line sku "$ac")"
    p="$(rc "$mq" XPENDING orders/q0 mygroup)"
    assert_eq "the claimant now owns all pending rows" "c3" "$(nth 4 "$p")"
    ac="$(rc "$mq" XAUTOCLAIM orders/q0 mygroup c9 60000 0-0 COUNT 10)"
    assert_eq "a 60s idle gate claims nothing (cursor 0-0)" "0-0" \
        "$(nth 1 "$ac")"
    assert_eq "a 60s idle gate claims no entries" "0" "$(count_line sku "$ac")"
    assert_eq "final XACK drains the group" "3" \
        "$(rc "$mq" XACK orders/q0 mygroup "$id1" "$id2" "$id3")"
    assert_eq "empty PEL summary is [0, nil, nil]" "0" \
        "$(nth 1 "$(rc "$mq" XPENDING orders/q0 mygroup)")"

    # ---- (e) ORDERED group: strict serial + exclusive ownership ---------
    local od=od/q0 o
    rc "$mq" XADD "$od" 1-1 f o1 >/dev/null
    rc "$mq" XADD "$od" 1-2 f o2 >/dev/null
    rc "$mq" XADD "$od" 1-3 f o3 >/dev/null
    assert_eq "XGROUP CREATE ... ORDERED (default inflight 1)" "OK" \
        "$(rc "$mq" XGROUP CREATE "$od" og 0-0 MKSTREAM ORDERED)"
    assert_contains "INFLIGHT without ORDERED is a syntax error" \
        "ERR syntax error" \
        "$(rc "$mq" XGROUP CREATE "$od" ogx 0-0 MKSTREAM INFLIGHT 2)"
    o="$(rc "$mq" XREADGROUP GROUP og cA STREAMS "$od" '>')"
    assert_eq "strict serial: the first read yields ONE entry" "1" \
        "$(count_line f "$o")"
    assert_eq "the first entry is the oldest id" "1-1" "$(nth 2 "$o")"
    assert_eq "window full: the owner re-reading gets the nil array" "" \
        "$(rc "$mq" XREADGROUP GROUP og cA STREAMS "$od" '>')"
    assert_eq "queue ownership is exclusive: cB is fenced out" "" \
        "$(rc "$mq" XREADGROUP GROUP og cB STREAMS "$od" '>')"
    assert_eq "XACK frees the inflight window" "1" \
        "$(rc "$mq" XACK "$od" og 1-1)"
    o="$(rc "$mq" XREADGROUP GROUP og cA STREAMS "$od" '>')"
    assert_eq "after the ack the OWNER gets the next entry in order" "1-2" \
        "$(nth 2 "$o")"
    assert_eq "cB is still fenced (fresh 30s lease, no steal)" "" \
        "$(rc "$mq" XREADGROUP GROUP og cB STREAMS "$od" '>')"
    assert_eq "XGROUP CREATE ... ORDERED INFLIGHT 2 widens the window" "OK" \
        "$(rc "$mq" XGROUP CREATE "$od" og2 0-0 MKSTREAM ORDERED INFLIGHT 2)"
    o="$(rc "$mq" XREADGROUP GROUP og2 cC COUNT 5 STREAMS "$od" '>')"
    assert_eq "INFLIGHT 2 prefetches exactly 2 entries" "2" \
        "$(count_line f "$o")"
    assert_not_contains "INFLIGHT 2 stops before the third entry" "1-3" "$o"

    # ---- (f) XINFO STREAM counts + XINFO GROUPS fields ------------------
    local si
    si="$(rc "$mq" XINFO STREAM orders/q0)"
    assert_contains "XINFO STREAM reports the retained length" "length" "$si"
    assert_contains "XINFO STREAM length matches XLEN" "4" "$si"
    assert_contains "XINFO STREAM reports the group count" "groups" "$si"
    assert_contains "XINFO STREAM on a missing stream errors" "no such key" \
        "$(rc "$mq" XINFO STREAM nope/q0)"
    # Plain redis-cli now that the array header is honest (13 elements);
    # `timeout` guards the old hang class (a miscounted header blocks
    # redis-cli forever -- better a failed assert than a stuck suite).
    si="$(timeout 10 redis-cli -h "$E2E_HOST" -p "$(node_resp "$mq")"         --raw XINFO GROUPS "$od" 2>&1)"
    assert_contains "XINFO GROUPS lists the ordered group" "og" "$si"
    assert_contains "XINFO GROUPS exposes the ordered flag" "ordered" "$si"
    assert_contains "XINFO GROUPS exposes the inflight window" "inflight" "$si"
    assert_contains "XINFO GROUPS exposes the committed watermark" \
        "committed-id" "$si"
    assert_contains "XINFO GROUPS exposes the live owner slot" "owner" "$si"
    # redis-cli --raw prints the RESP integer without its ':' prefix.
    assert_contains "the ordered group is flagged ordered=1" "$(printf 'ordered\n1')" "$si"

    # ---- (g) DLQ + MAXDELIVERY: poison messages dead-letter out --------
    local ds=dl/q0 cl dr
    assert_contains "a DLQ target without a MAXDELIVERY cap is a syntax error" \
        "ERR syntax error" \
        "$(rc "$mq" XGROUP CREATE "$ds" gx 0-0 MKSTREAM DLQ dl/other)"
    assert_eq "MAXDELIVERY wants n >= 1" \
        "ERR value is not an integer or out of range" \
        "$(rc "$mq" XGROUP CREATE "$ds" gx 0-0 MKSTREAM MAXDELIVERY 0)"
    assert_eq "XGROUP CREATE ... MAXDELIVERY 2 (no explicit DLQ name)" "OK" \
        "$(rc "$mq" XGROUP CREATE "$ds" dg 0-0 MKSTREAM MAXDELIVERY 2)"
    rc "$mq" XADD "$ds" 1-1 sku poison >/dev/null
    rc "$mq" XADD "$ds" 1-2 sku fine >/dev/null
    cl="$(rc "$mq" XREADGROUP GROUP dg c1 COUNT 1 STREAMS "$ds" '>')"
    assert_eq "COUNT 1 hands only the first (poison) entry to c1" "1" \
        "$(count_line poison "$cl")"
    cl="$(rc "$mq" XCLAIM "$ds" dg c2 0 1-1)"
    assert_contains "claim #2 (times 1->2, still within the cap) delivers" \
        "1-1" "$cl"
    cl="$(rc "$mq" XCLAIM "$ds" dg c2 0 1-1)"
    assert_eq "claim #3 (times would pass the cap 2) transfers: no entry" \
        "" "$cl"
    assert_eq "the transfer empties the group PEL (summary 0)" "0" \
        "$(nth 1 "$(rc "$mq" XPENDING "$ds" dg)")"
    assert_eq "the default DLQ target is the literal <stream>/dlq" "1" \
        "$(rc "$mq" XLEN "$ds/dlq")"
    dr="$(rc "$mq" XRANGE "$ds/dlq" - +)"
    assert_contains "the DLQ entry keeps the original payload" "poison" "$dr"
    assert_contains "the DLQ entry traces the source group" "__dlq_group" "$dr"
    assert_contains "the DLQ entry traces the source consumer" \
        "__dlq_consumer" "$dr"
    assert_contains "the DLQ entry traces the delivery count" "__dlq_times" "$dr"
    assert_contains "the DLQ entry traces the source stream" "__dlq_src" "$dr"
    # The DLQ is a plain stream: consume + ack it independently.
    assert_eq "a group can subscribe to the DLQ itself" "OK" \
        "$(rc "$mq" XGROUP CREATE "$ds/dlq" dgq 0-0 MKSTREAM)"
    cl="$(rc "$mq" XREADGROUP GROUP dgq dc1 STREAMS "$ds/dlq" '>')"
    assert_contains "the DLQ consumer receives the dead-lettered entry" \
        "1-1" "$cl"
    assert_contains "trace fields ride along for the DLQ consumer" \
        "__dlq_group" "$cl"
    assert_eq "XACK drains the DLQ group" "1" \
        "$(rc "$mq" XACK "$ds/dlq" dgq 1-1)"
    assert_eq "the DLQ group PEL is empty again" "0" \
        "$(nth 1 "$(rc "$mq" XPENDING "$ds/dlq" dgq)")"

    # ---- (h) XTRIM MINID: id threshold, LIMIT cap, time window ---------
    local ts=tm/q0 tr
    rc "$mq" XADD "$ts" 1-1 f v1 >/dev/null
    rc "$mq" XADD "$ts" 1-2 f v2 >/dev/null
    rc "$mq" XADD "$ts" 1-3 f v3 >/dev/null
    rc "$mq" XADD "$ts" 1-4 f v4 >/dev/null
    rc "$mq" XADD "$ts" 1-5 f v5 >/dev/null
    assert_eq "XTRIM MINID = drops entries strictly below the id (2 of 5)" \
        "2" "$(rc "$mq" XTRIM "$ts" MINID = 1-3)"
    tr="$(rc "$mq" XRANGE "$ts" - +)"
    assert_eq "the boundary id itself survives as the first entry" "1-3" \
        "$(nth 1 "$tr")"
    assert_not_contains "strictly-below ids are gone" "1-2" "$tr"
    assert_eq "the approximate flag ~ is accepted and acts exactly (=)" "0" \
        "$(rc "$mq" XTRIM "$ts" MINID '~' 1-3)"
    # LIMIT caps one round: the remainder is a later call's work.
    local t2=tm/q1
    rc "$mq" XADD "$t2" 1-1 f v1 >/dev/null
    rc "$mq" XADD "$t2" 1-2 f v2 >/dev/null
    rc "$mq" XADD "$t2" 1-3 f v3 >/dev/null
    rc "$mq" XADD "$t2" 1-4 f v4 >/dev/null
    rc "$mq" XADD "$t2" 1-5 f v5 >/dev/null
    assert_eq "LIMIT 1 caps the trim to one victim" "1" \
        "$(rc "$mq" XTRIM "$t2" MINID = 1-3 LIMIT 1)"
    tr="$(rc "$mq" XRANGE "$t2" - +)"
    assert_eq "the capped trim leaves the next-below id as the head" "1-2" \
        "$(nth 1 "$tr")"
    assert_eq "XLEN after the capped trim" "4" "$(rc "$mq" XLEN "$t2")"
    # Time-window retention: ids are `<ms timestamp>-<seq>`, so
    # `XTRIM <s> MINID <cutoff_ms>-0` keeps exactly the entries whose
    # arrival time is >= cutoff -- one periodic command, no per-message
    # index (the sliding-window idiom from tests/lite_trim_minid_e2e.rs).
    local t3=tm/q2
    rc "$mq" XADD "$t3" 1000-0 f old >/dev/null
    rc "$mq" XADD "$t3" 2000-0 f edge >/dev/null
    rc "$mq" XADD "$t3" 3000-0 f new >/dev/null
    assert_eq "MINID <ms>-0 keeps the time window (drop < cutoff)" "1" \
        "$(rc "$mq" XTRIM "$t3" MINID 2000-0)"
    tr="$(rc "$mq" XRANGE "$t3" - +)"
    assert_eq "the window boundary entry survives" "2000-0" "$(nth 1 "$tr")"
    assert_not_contains "pre-window entries are reaped" "old" "$tr"

    # ---- (i) delayed messages: DELAY staging -> due exchange ----------
    # DELAY 0 is the plain path: nothing staged, synchronously visible.
    local dly=dm/q0 rid rng blk fresh
    rc "$mq" XADD "$dly" '*' DELAY 0 sku now >/dev/null
    assert_eq "DELAY 0 = no delay: visible immediately" "1" \
        "$(rc "$mq" XLEN "$dly")"
    # A 4s message: absent from every read path before due...
    rid="$(rc "$mq" XADD "$dly" '*' DELAY 4000 sku later)"
    assert_eq "XLEN excludes the staged row before due" "1" \
        "$(rc "$mq" XLEN "$dly")"
    assert_not_contains "XRANGE is blind to the staged row before due" \
        "later" "$(rc "$mq" XRANGE "$dly" - +)"
    # ...and a BLOCK reader parked on `$` BEFORE due must be WOKEN by the
    # exchange (not by its own timeout: a timed-out BLOCK prints nothing,
    # so seeing the payload below proves the notify).
    blk="$E2E_WORKDIR/block_wake.out"
    ( timeout 20 redis-cli -h "$E2E_HOST" -p "$(node_resp "$mq")" --raw \
        XREAD BLOCK 9000 STREAMS "$dly" '$' >"$blk" 2>/dev/null ) &
    blkpid=$!
    rng="$(until_grep later 12 rc "$mq" XRANGE "$dly" - +)"
    assert_contains "the staged row exchanges into the stream after due" \
        "later" "$rng"
    assert_eq "the exchange is a real entry (XLEN counts it)" "2" \
        "$(rc "$mq" XLEN "$dly")"
    # XRANGE --raw prints 3 lines per entry: [id sku now][id sku later].
    fresh="$(nth 4 "$rng")"
    assert_not_contains "the XADD reply id was a reservation token only" \
        "$rid" "$rng"
    # Wait ONLY for the parked reader's job: a bare `wait` would also
    # block on the never-exiting rdb cluster nodes and hang the scenario.
    wait "$blkpid" 2>/dev/null || true # the woken reader exits on its own
    assert_contains "the parked BLOCK reader was woken by the exchange" \
        "$dly" "$(cat "$blk" 2>/dev/null)"
    assert_contains "the woken reader received the delayed payload" \
        "later" "$(cat "$blk" 2>/dev/null)"
    assert_contains "the woken reader got the FRESH exchange id" "$fresh" \
        "$(cat "$blk" 2>/dev/null)"
    # RENAME carries staged rows with the stream family (same-slot pair
    # from tests/lite_delay_e2e.rs): the row exchanges at the NEW name.
    local rsrc=t3107/q5 rdst=t43847/q5
    rc "$mq" XADD "$rsrc" '*' DELAY 2500 evt moved >/dev/null
    assert_eq "RENAME carries the staged 0x1D row with the family" "OK" \
        "$(rc "$mq" RENAME "$rsrc" "$rdst")"
    rng="$(until_grep moved 10 rc "$mq" XRANGE "$rdst" - +)"
    assert_contains "the carried row exchanges at the new name" "moved" "$rng"
    assert_eq "the old name no longer exists" "0" "$(rc "$mq" XLEN "$rsrc")"
    # Option negatives (src/lite/delay.rs::split_delay / deadline check).
    assert_eq "DELAY refuses a non-numeric argument" \
        "ERR value is not an integer or out of range" \
        "$(rc "$mq" XADD "$dly" '*' DELAY soon sku x)"
    assert_eq "a DELAY past the u64 deadline is refused, not wrapped" \
        "ERR delay deadline overflow" \
        "$(rc "$mq" XADD "$dly" '*' DELAY 18446744073709551615 sku x)"

    # ---- (j) P3 W2: NOMKSTREAM / XINFO FULL / XCLAIM RETRYCOUNT -------
    # NB: lite streams are physically keyed under the PARENT topic's
    # CRC16 slot (src/lite/model.rs::stream_prefix), while EXISTS/TYPE
    # route on the FULL stream name's slot -- a slot-routed EXISTS can
    # never observe a lite stream: it answers 0 even after creation
    # (verified on a scratch node), or MOVED when another node owns the
    # child slot. The node-local existence oracle is the whitelisted
    # KEYS (local keyspace scan): absent -> empty output, created ->
    # the key itself; the contrast below keeps the negative airtight.
    local nm=nmk1 fs=full/q0 si cl pend
    assert_eq "XADD NOMKSTREAM on a missing key replies nil" "" \
        "$(rc "$mq" XADD "$nm/q0" NOMKSTREAM '*' sku ghost)"
    assert_eq "the missing key stays absent (XLEN 0)" "0" \
        "$(rc "$mq" XLEN "$nm/q0")"
    assert_eq "no physical key materialized (KEYS finds nothing)" "" \
        "$(rc "$mq" KEYS "$nm/q0")"
    assert_eq "plain XADD creates the stream (echoes the id)" "5-1" \
        "$(rc "$mq" XADD "$nm/q0" 5-1 sku real)"
    assert_eq "NOMKSTREAM only suppresses CREATION: it appends on a live stream" \
        "5-2" "$(rc "$mq" XADD "$nm/q0" NOMKSTREAM 5-2 sku real2)"
    assert_eq "KEYS now sees the stream (the oracle is not vacuous)" "$nm/q0" \
        "$(rc "$mq" KEYS "$nm/q0")"
    assert_eq "the created stream holds both entries" "2" \
        "$(rc "$mq" XLEN "$nm/q0")"

    # XINFO STREAM FULL: the Redis-7 deep view, entries + groups nested.
    rc "$mq" XADD "$fs" 7-1 sku v1 >/dev/null
    rc "$mq" XADD "$fs" 7-2 sku v2 >/dev/null
    rc "$mq" XADD "$fs" 7-3 sku v3 >/dev/null
    assert_eq "XGROUP CREATE for the FULL-view probe" "OK" \
        "$(rc "$mq" XGROUP CREATE "$fs" fg 0-0 MKSTREAM)"
    cl="$(rc "$mq" XREADGROUP GROUP fg fc1 COUNT 1 STREAMS "$fs" '>')"
    assert_contains "one entry read unacked (PEL seeded at fc1)" "v1" "$cl"
    si="$(rc "$mq" XINFO STREAM "$fs" FULL)"
    assert_contains "FULL carries the length field" "length" "$si"
    assert_eq "FULL length value (flattened pair layout)" "3" "$(nth 2 "$si")"
    assert_eq "FULL last-generated-id is the newest entry" "7-3" \
        "$(nth 4 "$si")"
    assert_contains "FULL carries the entries list" "entries" "$si"
    assert_contains "the entries list nests the oldest entry frame" "v1" "$si"
    assert_contains "the entries list nests the newest entry frame" "v3" "$si"
    assert_contains "FULL carries the groups list" "groups" "$si"
    assert_contains "the groups section nests the group name" "fg" "$si"
    assert_contains "the group nests its consumer roster" "fc1" "$si"
    assert_contains "the group nests its pending list" "pending" "$si"
    si="$(rc "$mq" XINFO STREAM "$fs" FULL COUNT 1)"
    assert_contains "FULL COUNT keeps the newest entry" "v3" "$si"
    assert_not_contains "FULL COUNT caps the tail entries (older dropped)" \
        "v1" "$si"

    # XCLAIM RETRYCOUNT replaces the PEL delivery counter outright; a
    # later hint-less claim BUMPS from the replaced value (claim.rs).
    cl="$(rc "$mq" XCLAIM "$fs" fg fc2 0 7-1 RETRYCOUNT 3)"
    assert_contains "XCLAIM hands the full entry frame to the claimant" "v1" "$cl"
    pend="$(rc "$mq" XPENDING "$fs" fg - + 10)"
    assert_eq "the claim moved the PEL row to fc2" "fc2" "$(nth 2 "$pend")"
    assert_eq "RETRYCOUNT 3 replaced the delivery counter" "3" "$(nth 4 "$pend")"
    assert_eq "XPENDING summary total unchanged by the claim" "1" \
        "$(nth 1 "$(rc "$mq" XPENDING "$fs" fg)")"
    cl="$(rc "$mq" XCLAIM "$fs" fg fc1 0 7-1)"
    assert_contains "a hint-less claim still delivers the entry" "v1" "$cl"
    assert_eq "a later hint-less claim BUMPS from the replaced value" "4" \
        "$(nth 4 "$(rc "$mq" XPENDING "$fs" fg - + 10)")"

    e2e_finish "$scenario"
}

main "$@"
