#!/usr/bin/env bash
# scenario_lite_mq.sh - e2e: Lite MQ (RocketMQ-5.5-Lite semantics on the
# Redis Streams verbs) driven by REAL redis-cli against the 3-node rdb
# cluster assembled by env.sh: first CLI-suite coverage of what only the
# Rust tests pin (tests/lite_*_e2e.rs). Steps: (a) produce (bare-parent
# queue pick, explicit queue, XLEN, XRANGE COUNT), (b) XGROUP CREATE +
# XREADGROUP `>` COUNT handoff, (c) XPENDING summary/range + XACK,
# (d) second consumer + XAUTOCLAIM takeover, (e) ORDERED strict-serial
# group + INFLIGHT knob, (f) XINFO STREAM / XINFO GROUPS.
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

# env.sh installs `set -uo pipefail` and the assertion helpers. Deliberately
# NO `set -e`: assertion failures count into E2E_FAILS so the scenario
# always runs to its e2e_finish summary.
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

# rdb gates every pre-AUTH command behind AUTH (env.sh exports this too).
export REDISCLI_AUTH="${RDB_E2E_TOKEN}"

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

    e2e_finish "$scenario"
}

main "$@"
