#!/usr/bin/env bash
# scenario_redis_session.sh - e2e: "session cache / leaderboard" story driven
# by REAL CLIs (redis-cli 6.0.16, iredis 1.16.1) against a 3-node rdb cluster
# assembled by env.sh. Story steps:
#   a. session write/readback (SET/GET, values with spaces + non-ASCII bytes)
#   b. TTL: SET ... EX 2 -> TTL>0 -> sleep past expiry -> GET nil, TTL -2
#   c. HSET user profile -> HGET/HGETALL
#   d. ZADD leaderboard -> ZRANGE ... REV WITHSCORES ordering
#   e. hash tag: {sess:u1001}:a and {sess:u1001}:b share one slot
#   f. MOVED: foreign-slot SET on node1 -> text format -> redis-cli -c follows
#   g. MULTI/EXEC batch of session writes -> read every key back
# plus a compatibility smoke (source-confirmed probe commands only).
#
# Semantics verified against source/tests BEFORE writing this script:
# - AUTH gate is unconditional: pre-auth every command (PING included)
#   answers "-ERR: NOAUTH" (src/resp/conn.rs:255-267). Therefore this script
#   exports REDISCLI_AUTH right after sourcing env.sh: it authenticates every
#   redis-cli call here AND the bare-PING readiness probes env.sh itself makes.
# - SET key val EX px supported (src/command/string_opts.rs:83-99).
# - TTL/PTTL: missing key -2, no expiry -1, expired key -2 (lazy purge on
#   read) -- src/command/keys_core.rs:53-60, tests/expire_e2e.rs:115-123.
# - Hash tag = FIRST {...} group, empty tag means no tag (src/hash.rs:59-81);
#   slot = CRC16(tag) % 16384 (src/hash.rs:41-46). CLUSTER KEYSLOT exists and
#   returns that slot (src/command/cluster.rs:48,61-69; cluster_slot tests:
#   keyslot "key"=12539, "foo"=12182).
# - MOVED text is "MOVED <slot> <addr>" (src/router.rs:74-77); addr is the
#   owner's RESP bind from the instances list (src/command/mod.rs:300-312,
#   tests/process_cluster_e2e.rs:129-146 "MOVED addr must be one of the
#   cluster binds"). GET is routed like SET, so a plain GET on a non-owner
#   MOVEDs too (process_cluster_e2e.rs:169); reads go through the owner or
#   via redis-cli -c following the redirect.
# - Slot ownership bands: equal split, 16384/3 = 5461 per node, instances in
#   CLUSTER INIT order, LAST node absorbs the remainder (src/router.rs:16-55).
# - MULTI/EXEC: enabled by conf (env.sh yaml sets tx.enabled=true);
#   queue-time validation answers "+QUEUED" (src/resp/conn.rs:351) and
#   redirects dirty transactions when a queued key MOVEDs
#   (src/resp/conn.rs:322-334) -- so the MULTI must be sent to the OWNER of
#   the transaction slot. EXEC frames all buffered replies as one "*N" array
#   (src/tx/exec.rs:24-30,94-101; tests/tx_e2e.rs:126 "*2 [+OK, $2 v1]").
# - ZADD replies the count of NEW members (src/command/zset_cmd.rs:59-60);
#   ZREVRANGE ... WITHSCORES interleaves member/score as bulk strings with
#   scores rendered like Rust f64 Display ("98.5", "72" not "72.0")
#   (src/command/zset_range.rs:68-88, src/command/zset_util.rs:124-126).
# - HGETALL of a missing key is an empty array (src/command/hash_tests.rs:125).
# - Probe-command smoke: rdb has NO top-level INFO/COMMAND (src/command/
#   mod.rs dispatch -> "ERR unknown command '<name>'", mod.rs:364-370), so
#   the smoke uses source-confirmed CLUSTER INFO (cluster.rs:44,187-215)
#   and CLUSTER KEYSLOT, then proves the connection still PINGs.
# - Cluster metadata: env.sh does NOT run CLUSTER INIT; the topology only
#   materializes after "CLUSTER INIT <resp_bind0>,<bind1>,<bind2>" on the
#   raft leader (src/command/cluster.rs:292-325; tests/common/mod.rs:533-542)
#   and reaches every node via the 3s topology ticker (src/main.rs:79-111),
#   so this script polls CLUSTER NODES on all three nodes before routing
#   assertions. Leader is located via "raft nodes" reporting "[Leader]"
#   (src/command/raft_cmd.rs:44-49,149).
# - CLI call shapes (verified empirically against a canned RESP server):
#   * redis-cli on a pipe defaults to raw output: values print bare, nil GET
#     prints an empty line, integers bare, errors print their text (e.g.
#     "MOVED 42 host:port") and exit code stays 0 (1 only when the TCP
#     connect is refused). --raw is passed explicitly so a manual tty run
#     behaves identically.
#   * redis-cli -c follows MOVED and prints the owner's final reply.
#   * pipelined inline commands support quoted values (SET k "a b").
#   * iredis 1.16.1 has NO pipe/batch subcommand (iredis --help: no such
#     option) and its one-shot "iredis -p P CMD" form prints NOTHING on a
#     non-tty stdout. Working form (verified): pipe ONE command line on
#     stdin with an explicit password: printf 'GET k\n' | iredis -p P -a
#     TOKEN --no-raw. This script uses exactly that, one command per call.

# env.sh installs `set -uo pipefail` and the assertion helpers. Deliberately
# NO `set -e`: assertions count failures in E2E_FAILS instead of aborting,
# so the scenario always runs to its e2e_finish summary.
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

# rdb requires AUTH <raft_token> before ANY command (see header). One export
# covers every redis-cli invocation of this process, including the bare PING
# probes env.sh makes while waiting for readiness.
export REDISCLI_AUTH="${RDB_E2E_TOKEN}"

# Mandated shared client prefix; E2E_HOST has no spaces, word splitting of
# $RC is intentional. rc() appends the per-node port and forces --raw so the
# shapes match the verified raw-mode output even on a tty.
RC="redis-cli -h $E2E_HOST -p"

# One redis-cli call against node <idx>'s RESP port. args... follow.
rc () {
    local idx=$1
    shift
    $RC "$(node_resp "$idx")" --raw "$@" 2>/dev/null
}

# redis-cli -c (follow MOVED redirects) against node <idx>. NOTE: -c must
# come after the -p <port> pair -- "$RC -c $port" would let -p consume "-c"
# as the port number and the call would fail to connect.
rcc () {
    local idx=$1
    shift
    $RC "$(node_resp "$idx")" --raw -c "$@" 2>/dev/null
}

# iredis call: pipe ONE command line on stdin (see header for why the
# one-shot argument form prints nothing). args: <idx> <command line>
iredis1 () {
    local idx=$1
    local cmdline=$2
    printf '%s\n' "$cmdline" \
        | iredis -h "$E2E_HOST" -p "$(node_resp "$idx")" \
            -a "$RDB_E2E_TOKEN" --no-raw 2>/dev/null
}

# Index of the node whose "raft nodes" reports "[Leader]" (empty if none).
find_leader_idx () {
    local i out
    for i in 0 1 2; do
        out="$(rc "$i" raft nodes)"
        case "$out" in
        *"[Leader]"*) echo "$i"; return 0 ;;
        esac
    done
    return 1
}

# Poll every node's CLUSTER NODES until all three RESP binds are listed
# (topology ticker is 3s). args: timeout seconds -> 0 when converged.
wait_topology_converged () {
    local deadline=$((SECONDS + $1)) i binds
    binds="$E2E_HOST:$(node_resp 0) $E2E_HOST:$(node_resp 1) $E2E_HOST:$(node_resp 2)"
    while [ "$SECONDS" -lt "$deadline" ]; do
        local ok=1
        for i in 0 1 2; do
            local nodes
            nodes="$(rc "$i" cluster nodes)"
            local b
            for b in $binds; do
                case "$nodes" in
                *"$b"*) : ;;
                *) ok=0 ;;
                esac
            done
        done
        [ "$ok" = "1" ] && return 0
        sleep 1
    done
    return 1
}

# Plain (non -c) SET probe: prints LOCAL when node <idx> owns <key>, the
# owner's "host:port" when it answers MOVED, "UNEXPECTED:<reply>" otherwise.
# args: <idx> <key>
owner_probe () {
    local idx=$1 key=$2 reply
    reply="$(rc "$idx" SET "$key" probe)"
    case "$reply" in
    OK) echo LOCAL ;;
    MOVED*)
        # reply is "MOVED <slot> <host:port>"; the owner is field 3.
        printf '%s\n' "$reply" | awk '{print $3}'
        ;;
    *) echo "UNEXPECTED:$reply" ;;
    esac
}

# True when $1 is a (possibly negative) decimal integer.
is_int () {
    case "$1" in
    ''|*[!0-9-]*) return 1 ;;
    *) return 0 ;;
    esac
}

# Map an owner "host:port" back to its node index (empty when unknown).
idx_of_addr () {
    local addr=$1 i
    for i in 0 1 2; do
        [ "$addr" = "$E2E_HOST:$(node_resp "$i")" ] && { echo "$i"; return 0; }
    done
    return 1
}

# Home node of <key>: a plain GET probe answers nil (empty) on the owner and
# "MOVED <slot> <addr>" elsewhere, so this never mutates data. Must run
# AFTER the topology converged. Falls back to node0 on anything unexpected.
key_home_node () {
    local key=$1 reply addr j
    reply="$(rc 0 GET "$key")"
    case "$reply" in
    MOVED*)
        addr="$(printf '%s\n' "$reply" | awk '{print $3}')"
        j="$(idx_of_addr "$addr")"
        echo "${j:-0}"
        ;;
    *) echo 0 ;;
    esac
}

main () {
    local scenario="redis_session"
    e2e_init "$scenario"

    e2e_start_cluster || {
        echo "FATAL: cluster bring-up failed; aborting scenario" >&2
        exit 1
    }

    # ---- cluster metadata: leader + CLUSTER INIT + topology convergence --
    local leader init_reply
    leader="$(find_leader_idx)" || {
        echo "FATAL: no raft leader reported by 'raft nodes'" >&2
        exit 1
    }
    init_reply="$(rc "$leader" cluster init \
        "$E2E_HOST:$(node_resp 0),$E2E_HOST:$(node_resp 1),$E2E_HOST:$(node_resp 2)")"
    assert_eq "cluster init on leader (node$leader) replies done" "done" "$init_reply"
    if wait_topology_converged 30; then
        assert_eq "all 3 nodes list all 3 binds in CLUSTER NODES" "ok" "ok"
    else
        assert_eq "all 3 nodes list all 3 binds in CLUSTER NODES" "ok" "timeout"
    fi

    # ---- compatibility smoke: source-confirmed probe commands only -------
    local cinfo ping_after
    cinfo="$(rc 0 cluster info)"
    assert_contains "CLUSTER INFO reports cluster_state:true" \
        "cluster_state:true" "$cinfo"
    assert_contains "CLUSTER INFO reports cluster_known_nodes:3" \
        "cluster_known_nodes:3" "$cinfo"
    ping_after="$(rc 0 PING)"
    assert_eq "connection still PINGs after probe commands" "PONG" "$ping_after"

    # ---- (a) session write/readback --------------------------------------
    # Writes and reads go to each key's HOME node (slot ownership bands are
    # equal splits in CLUSTER INIT order, src/router.rs:16-55); a plain
    # non-owner call would answer MOVED. Sections (e)/(f) cover the tagged
    # and redirected paths explicitly.
    local v_space="hello rdb world"
    # Non-ASCII value built from UTF-8 byte escapes: the scenario file itself
    # stays pure ASCII while the payload carries multibyte text.
    local v_utf8
    v_utf8="$(printf '\xe4\xbd\xa0\xe5\xa5\xbd rdb')"
    local h_login h_cn h_pipe
    h_login="$(key_home_node sess:login:alice)"
    h_cn="$(key_home_node sess:cn:token)"
    assert_eq "SET session with spaces replies OK" "OK" \
        "$(rc "$h_login" SET sess:login:alice "$v_space")"
    assert_eq "GET session with spaces round-trips" "$v_space" \
        "$(rc "$h_login" GET sess:login:alice)"
    assert_eq "SET non-ASCII session value replies OK" "OK" \
        "$(rc "$h_cn" SET sess:cn:token "$v_utf8")"
    assert_eq "GET non-ASCII session value round-trips" "$v_utf8" \
        "$(rc "$h_cn" GET sess:cn:token)"
    # Pipeline form with an inline quoted value (redis-cli inline parser).
    local pipe_out
    h_pipe="$(key_home_node sess:pipe:note)"
    pipe_out="$(printf 'SET sess:pipe:note "two words here"\nGET sess:pipe:note\n' \
        | $RC "$(node_resp "$h_pipe")" --raw 2>/dev/null)"
    assert_contains "pipelined SET+GET with quoted value" "two words here" "$pipe_out"

    # ---- (b) TTL: SET ... EX 2 -> expire -> nil GET, TTL -2 ---------------
    local h_tok
    h_tok="$(key_home_node sess:tok:web)"
    assert_eq "SET with EX replies OK" "OK" "$(rc "$h_tok" SET sess:tok:web t1 EX 2)"
    local ttl pttl
    ttl="$(rc "$h_tok" TTL sess:tok:web)"
    if is_int "$ttl" && [ "$ttl" -ge 1 ] && [ "$ttl" -le 2 ]; then
        assert_eq "TTL right after EX 2 is 1..2 (got $ttl)" "ok" "ok"
    else
        assert_eq "TTL right after EX 2 is 1..2 (got $ttl)" "1..2" "$ttl"
    fi
    pttl="$(rc "$h_tok" PTTL sess:tok:web)"
    if is_int "$pttl" && [ "$pttl" -gt 0 ] && [ "$pttl" -le 2000 ]; then
        assert_eq "PTTL right after EX 2 is (0,2000] ms (got $pttl)" "ok" "ok"
    else
        assert_eq "PTTL right after EX 2 is (0,2000] ms (got $pttl)" "0<ms<=2000" "$pttl"
    fi
    assert_eq "GET before expiry returns the value" "t1" "$(rc "$h_tok" GET sess:tok:web)"
    sleep 3
    assert_eq "GET after expiry returns nil (empty line in raw mode)" "" \
        "$(rc "$h_tok" GET sess:tok:web)"
    assert_eq "TTL of expired key is -2 (lazy purge)" "-2" "$(rc "$h_tok" TTL sess:tok:web)"

    # ---- (c) HSET user profile -> HGET / HGETALL --------------------------
    local h_prof
    h_prof="$(key_home_node sess:profile:u1001)"
    assert_eq "HSET two fields replies new-field count 2" "2" \
        "$(rc "$h_prof" HSET sess:profile:u1001 city Beijing os linux)"
    assert_eq "HGET reads one field back" "Beijing" \
        "$(rc "$h_prof" HGET sess:profile:u1001 city)"
    local hall want_hall
    hall="$(rc "$h_prof" HGETALL sess:profile:u1001 | tr '\n' ' ')"
    want_hall="city Beijing os linux "
    assert_eq "HGETALL returns flat field/value pairs" "$want_hall" "$hall"
    assert_contains "iredis HGET via stdin-pipe reads the field" "Beijing" \
        "$(iredis1 "$h_prof" 'HGET sess:profile:u1001 city')"

    # ---- (d) ZADD leaderboard -> ZRANGE ... REV WITHSCORES ordering ------
    # (rdb implements the Redis >= 6.2 surface: ZRANGE with REV, no
    # legacy ZREVRANGE alias.)
    local h_lb
    h_lb="$(key_home_node sess:lb:weekly)"
    assert_eq "ZADD four members replies 4" "4" \
        "$(rc "$h_lb" ZADD sess:lb:weekly 98.5 alice 72 bob 88 carol 61 dave)"
    local zrev want_zrev
    zrev="$(rc "$h_lb" ZRANGE sess:lb:weekly 0 -1 REV WITHSCORES | tr '\n' ' ')"
    want_zrev="alice 98.5 carol 88 bob 72 dave 61 "
    assert_eq "ZRANGE REV WITHSCORES is descending by score" "$want_zrev" "$zrev"
    assert_eq "iredis ZRANGE REV tops with the best member" "alice" \
        "$(iredis1 "$h_lb" 'ZRANGE sess:lb:weekly 0 0 REV')"

    # ---- (e) hash tag {sess:u1001} pins one slot --------------------------
    local ks_tag ks_a ks_b
    ks_tag="$(rc 0 CLUSTER KEYSLOT '{sess:u1001}')"
    ks_a="$(rc 0 CLUSTER KEYSLOT '{sess:u1001}:a')"
    ks_b="$(rc 0 CLUSTER KEYSLOT '{sess:u1001}:b')"
    assert_eq "CLUSTER KEYSLOT: tagged keys share one slot" "$ks_a" "$ks_b"
    assert_eq "CLUSTER KEYSLOT: slot equals CRC16 of the bare tag" "$ks_a" "$ks_tag"
    assert_eq "redis-cli -c SET of tagged key a succeeds" "OK" \
        "$(rcc 0 SET {sess:u1001}:a sa-v)"
    assert_eq "redis-cli -c SET of tagged key b succeeds" "OK" \
        "$(rcc 0 SET {sess:u1001}:b sb-v)"
    assert_eq "tagged key a reads back" "sa-v" "$(rcc 0 GET {sess:u1001}:a)"
    assert_eq "tagged key b reads back" "sb-v" "$(rcc 0 GET {sess:u1001}:b)"

    # ---- (f) MOVED: foreign-slot write on node1, -c follows the redirect --
    local found="" i probe_reply mv_slot mv_owner mv_owner_idx
    for i in $(seq 0 39); do
        probe_reply="$(owner_probe 1 "sess:move:$i")"
        case "$probe_reply" in
        "$E2E_HOST":*) found="sess:move:$i"; break ;; # MOVED -> owner address
        *) : ;;                                       # LOCAL / UNEXPECTED: keep looking
        esac
    done
    if [ -n "$found" ]; then
        assert_eq "found a key whose home slot node1 does not own" "ok" "ok"
    else
        assert_eq "found a key whose home slot node1 does not own" "a MOVED key" "none in 40 probes"
    fi
    # The plain probe already captured a "MOVED <slot> <addr>" reply: assert
    # its exact text shape and that the slot matches CLUSTER KEYSLOT.
    [ -n "$found" ] || found="sess:move:none" # keep later calls well-formed
    probe_reply="$(rc 1 SET "$found" probe)"
    case "$probe_reply" in
    MOVED\ [0-9]*\ "$E2E_HOST":[0-9]*)
        assert_eq "MOVED reply matches 'MOVED <slot> <host:port>'" "ok" "ok"
        ;;
    *)
        assert_eq "MOVED reply matches 'MOVED <slot> <host:port>'" "MOVED <slot> <host:port>" "$probe_reply"
        ;;
    esac
    mv_slot="$(printf '%s\n' "$probe_reply" | awk '{print $2}')"
    assert_eq "MOVED slot equals CLUSTER KEYSLOT of the key" \
        "$mv_slot" "$(rc 1 CLUSTER KEYSLOT "$found")"
    assert_eq "redis-cli -c SET follows MOVED and succeeds on the owner" "OK" \
        "$(rcc 1 SET "$found" moved-ok)"
    mv_owner="$(printf '%s\n' "$probe_reply" | awk '{print $3}')"
    mv_owner_idx="$(idx_of_addr "$mv_owner")"
    mv_owner_idx="${mv_owner_idx:-0}" # fall back to node0 if unmapped
    assert_eq "plain GET on the owning node ($mv_owner_idx) reads the value" \
        "moved-ok" "$(rc "$mv_owner_idx" GET "$found")"
    assert_eq "redis-cli -c GET from node1 follows the redirect to the value" \
        "moved-ok" "$(rcc 1 GET "$found")"

    # ---- (g) MULTI/EXEC batch of session writes ---------------------------
    # The transaction slot is 5213 ({sess:u1001}); queue-time validation
    # MOVED-aborts transactions sent to a non-owner, so locate the owner of
    # the tagged slot with a plain probe from node0 first.
    local tx_idx=0 tx_probe tx_owner
    tx_probe="$(owner_probe 0 '{sess:u1001}:probe')"
    if [ "$tx_probe" != "LOCAL" ]; then
        tx_owner="$(idx_of_addr "$tx_probe")"
        [ -n "$tx_owner" ] && tx_idx="$tx_owner"
    fi
    local tx_out queued
    tx_out="$(printf 'MULTI\nSET {sess:u1001}:cart "itemA itemB"\nHSET {sess:u1001}:profile city Shanghai\nEXEC\n' \
        | $RC "$(node_resp "$tx_idx")" --raw 2>/dev/null)"
    queued="$(printf '%s\n' "$tx_out" | grep -c '^QUEUED$' || true)"
    assert_eq "both writes inside MULTI answer +QUEUED" "2" "$queued"
    assert_contains "EXEC replies with the framed array contents" "OK" "$tx_out"
    assert_eq "session cart written by EXEC reads back" "itemA itemB" \
        "$(rc "$tx_idx" GET '{sess:u1001}:cart')"
    assert_eq "profile field written by EXEC reads back" "Shanghai" \
        "$(rc "$tx_idx" HGET '{sess:u1001}:profile' city)"

    e2e_finish "$scenario"
}

main "$@"
