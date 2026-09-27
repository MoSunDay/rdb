#!/usr/bin/env bash
# scenario_ha_failover.sh - e2e: `backup_target_map` backup takeover driven
# by a REAL redis-cli against 3 live rdb processes (first CLI-suite coverage
# of the path tests/backup_failover_e2e.rs drives through raw sockets):
#   1. yaml comes from env.sh's generator, but the `backup_target_map` is
#      CYCLED to the reference layout of config/conf.yaml: node i's entry
#      (keyed by node i's RAFT addr) maps src=resp_i -> target=backup_(i+1)%3.
#      env.sh's own generator points each node at ITS OWN backup port, and a
#      SIGKILLed node's backup listener dies with it -- a takeover needs a
#      target owned by a DIFFERENT, surviving process. Every yaml carries
#      the FULL 3-entry map, not just its own row: spawn_backup_map_init
#      seeds the raft keys from the LEADER's config only (src/rcache/ha.rs),
#      and the victim below is never the leader.
#   2. SIGKILL a non-leader. The leader's probe loop (5s tick, TCP connect,
#      5s timeout) marks the corpse FailedHeartbeatObservation and replaces
#      its RESP addr IN PLACE with the successor's backup addr inside the
#      raft key `cluster_slots_stable_instances`; after the 3s topology
#      resync ticker survivors answer `-MOVED <slot> <backup-addr>`.
#   3. the takeover listener serves reads from its OWN (separate, empty)
#      store -> nil, and rejects writes with -READONLY (src/command/readonly.rs).
#   4. restart the corpse: the next successful probe is a Resumed
#      observation, the swap-back restores the original instances list and
#      survivors MOVED the key to the victim's ORIGINAL resp port again.
#
# Semantics pinned from source BEFORE writing this script:
# - bands: per_node_slots = 16384/3 = 5461 (integer division,
#   src/topology.rs); the owner is the first index with
#   slot <= (index+1)*5461 and the LAST index absorbs the remainder, so
#   bands are 0-5461 / 5462-10922 / 10923-16383 for instances order
#   resp_0,resp_1,resp_2 (exactly what env.sh passes to CLUSTER INIT).
# - MOVED text is "MOVED <slot> <addr>" with the addr taken verbatim from
#   the instances list (src/router.rs, src/command/mod.rs), so the swapped
#   backup addr shows up in the redirect unmodified.
# - `RAFT GET <key>` (src/command/raft_cmd.rs) exposes the replicated FSM
#   values the observer mutates, mirrored into RaftState by the same 3s
#   ticker -- every read of it below is polled, never assumed.

# env.sh installs `set -uo pipefail`, the assertions and the cluster
# helpers; it also exports REDISCLI_AUTH=$RDB_E2E_TOKEN, which authenticates
# every redis-cli call here (AUTH gates ALL commands, src/resp/conn.rs).
source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

RC="redis-cli -h $E2E_HOST -p"

SLOTS=16384
PER_NODE=$((SLOTS / E2E_NODES))   # 5461 with E2E_NODES=3

# Inclusive band bounds of node <idx> for the 3-node instances order.
band_low () { echo $(( $1 * PER_NODE )); }
band_high () {
    if [ "$1" -eq $((E2E_NODES - 1)) ]; then
        echo $((SLOTS - 1))     # last node owns the remainder (src/router.rs)
    else
        echo $(( ( $1 + 1 ) * PER_NODE ))
    fi
}

rc () {  # one redis-cli call on node <idx> RESP port, stderr dropped
    local idx=$1; shift
    $RC "$(node_resp "$idx")" --raw "$@" 2>/dev/null
}

rc_all () {  # same, but KEEP error replies: MOVED prints as plain text
    local idx=$1; shift
    $RC "$(node_resp "$idx")" --raw "$@" 2>&1
}

bk () {  # one redis-cli call on node <idx>'s READ-ONLY backup listener
    local idx=$1; shift
    $RC "$(node_backup "$idx")" --raw "$@" 2>&1
}

is_int () { case "$1" in ''|*[!0-9]*) return 1 ;; *) return 0 ;; esac; }

# Full cyclic `backup_target_map` block: key = peer RAFT addr, value =
# "src=<peer resp>, target=<next node's backup bind>".
cyclic_map_block () {
    local i
    echo "backup_target_map:"
    for i in 0 1 2; do
        printf '  %s:%s:\n    src: %s:%s\n    target: %s:%s\n' \
            "$E2E_HOST" "$(node_raft_tcp "$i")" \
            "$E2E_HOST" "$(node_resp "$i")" \
            "$E2E_HOST" "$(node_backup $(( (i + 1) % 3 )))"
    done
}

# Swap the single-entry block env.sh generated in EVERY yaml for the cyclic
# one; the rest of each yaml is copied through verbatim.
cycle_backup_map () {
    local i f blk="$E2E_WORKDIR/cyclic_map.fragment"
    cyclic_map_block >"$blk"
    for i in 0 1 2; do
        f="$E2E_WORKDIR/conf_node$i.yaml"
        awk -v blk="$blk" '
            /^backup_target_map:/ {
                while ((l = getline ln < blk) > 0) print ln
                close(blk); skip = 1; next
            }
            skip && /^tx:/ { skip = 0 }
            !skip { print }
        ' "$f" >"$f.tmp" && mv "$f.tmp" "$f"
    done
}

# e2e_start_cluster's own sequence (env.sh), with the map cycled between
# yaml generation and the first spawn: bootstrap node0, join 1+2, wait for
# the RESP ports, LIFO banner, leader, CLUSTER INIT, sql registry, settle.
start_cluster_cycled () {
    local i
    [ -f "$RDB_BIN" ] || { echo "FATAL: $RDB_BIN missing (build first)" >&2; return 1; }
    for i in 0 1 2; do e2e_gen_yaml "$i" >/dev/null; done
    cycle_backup_map
    _e2e_spawn 0 bootstrap
    sleep 2
    _e2e_spawn 1 join
    sleep 1
    _e2e_spawn 2 join
    for i in 0 1 2; do
        _e2e_wait_ping "$i" || { echo "FATAL: node$i RESP not ready" >&2; return 1; }
    done
    _e2e_assert_lifo || return 1
    _e2e_wait_leader || { echo "FATAL: no leader elected" >&2; return 1; }
    _e2e_cluster_init || { echo "FATAL: CLUSTER INIT failed" >&2; return 1; }
    _e2e_wait_sql_registry || { echo "FATAL: sql_rpc registry incomplete" >&2; return 1; }
    sleep 1   # settle: band assignment replication + instance registration
}

# Poll node <idx> with `RAFT GET <key>` until the bulk value equals <want>.
wait_raft_get () { # <idx> <key> <want> [secs] -> 0/1
    local idx=$1 key=$2 want=$3 secs=${4:-$((E2E_WAIT_TIMEOUT * 3))} got=""
    local deadline=$((SECONDS + secs))
    while [ "$SECONDS" -lt "$deadline" ]; do
        got="$(rc "$idx" RAFT GET "$key")"
        [ "$got" = "$want" ] && return 0
        sleep 2
    done
    return 1
}

# Poll `GET <key>` on node <idx> until the reply is exactly
# "MOVED <slot> <want>" (redis-cli prints error replies as bare text in raw
# mode). LONG by design: detection needs one 5s probe tick + up to 5s of
# TCP-connect timeout, and the survivors only re-band after the 3s topology
# resync ticker replays the swapped instances list.
wait_moved () { # <idx> <key> <slot> <want-addr> [secs] -> 0/1
    local idx=$1 key=$2 slot=$3 want=$4 secs=${5:-$((E2E_WAIT_TIMEOUT * 3))} reply=""
    local deadline=$((SECONDS + secs))
    while [ "$SECONDS" -lt "$deadline" ]; do
        reply="$(rc_all "$idx" GET "$key" | tr -d '\r')"
        if [ "$reply" = "MOVED $slot $want" ]; then
            LAST_MOVED="$reply"
            return 0
        fi
        sleep 2
    done
    LAST_MOVED="$reply"
    return 1
}

# A key whose slot falls inside node <idx>'s band: CLUSTER KEYSLOT of a few
# fixed names, verified against the band bounds instead of trusted.
band_key () { # <leader-idx> <band-idx> -> prints the key, or fails
    local leader=$1 idx=$2 k slot
    local lo hi; lo="$(band_low "$idx")"; hi="$(band_high "$idx")"
    for k in ha:failover:seed ha:watchdog:seed ha:observer:seed; do
        slot="$(rc "$leader" CLUSTER KEYSLOT "$k")"
        is_int "$slot" || continue
        if [ "$slot" -ge "$lo" ] && [ "$slot" -le "$hi" ]; then
            echo "$k"
            return 0
        fi
    done
    return 1
}

main () {
    local scenario="ha_failover"
    e2e_init "$scenario"

    start_cluster_cycled || { echo "FATAL: cluster bring-up failed" >&2; return 1; }

    local leader
    leader="$(e2e_find_leader)" || { echo "FATAL: no leader" >&2; return 1; }

    # Victim: a NON-leader (killing the leader would trigger an election and
    # stop the probe loop we are testing) that is also never node0, whose
    # raft-http addr every restart joins through (env.sh hardcodes it).
    local victim
    if [ "$leader" -ne 1 ]; then victim=1; else victim=2; fi
    local succ=$(( (victim + 1) % 3 ))
    local succ_backup="$E2E_HOST:$(node_backup "$succ")"
    local victim_resp="$E2E_HOST:$(node_resp "$victim")"
    echo "leader=$leader victim=$victim successor=$succ backup=$succ_backup"

    # The leader seeded the cyclic map from its own config (1s init ticker):
    # "src,target" under the victim's raft-addr key.
    local map_key="backup_target_map_$E2E_HOST:$(node_raft_tcp "$victim")"    local map_key="backup_target_map_$E2E_HOST:$(node_raft_tcp "$victim")"
    if wait_raft_get "$leader" "$map_key" "$victim_resp,$succ_backup"; then
        echo "ok   - raft holds the victim's cyclic backup map entry"
    else
        _e2e_fail "RAFT GET $map_key: expected [$victim_resp,$succ_backup] got [$(rc "$leader" RAFT GET "$map_key")]"
    fi

    local key slot
    key="$(band_key "$leader" "$victim")" || { echo "FATAL: no key in band $victim" >&2; return 1; }
    slot="$(rc "$leader" CLUSTER KEYSLOT "$key")"
    if is_int "$slot" && [ "$slot" -ge "$(band_low "$victim")" ] && \
        [ "$slot" -le "$(band_high "$victim")" ]; then
        echo "ok   - CLUSTER KEYSLOT $slot of $key is inside victim $victim band $(band_low "$victim")-$(band_high "$victim")"
    else
        _e2e_fail "slot [$slot] of $key is outside victim $victim band $(band_low "$victim")-$(band_high "$victim")"
    fi

    # Baseline: the band owner is the victim itself, a survivor redirects to it.
    assert_eq "SET on the band owner (victim) writes the key" "OK" \
        "$(rc "$victim" SET "$key" ha-failover-value)"
    assert_eq "victim reads its own key back" "ha-failover-value" "$(rc "$victim" GET "$key")"
    assert_eq "survivor redirects to the victim's resp addr while it lives" \
        "MOVED $slot $victim_resp" "$(rc_all "$leader" GET "$key" | tr -d '\r')"

    # ---- SIGKILL the victim: band must move to the successor's backup ----
    local t_kill=$SECONDS
    e2e_kill_node "$victim" KILL

    if wait_moved "$leader" "$key" "$slot" "$succ_backup"; then
        echo "ok   - survivor MOVEDs the key to the successor's backup listener"
        assert_eq "MOVED reply names the backup addr verbatim" \
            "MOVED $slot $succ_backup" "$LAST_MOVED"
    else
        _e2e_fail "no takeover MOVED to $succ_backup within $((E2E_WAIT_TIMEOUT * 3))s"
    fi
    echo "info - kill -> MOVED-to-backup observed after $((SECONDS - t_kill))s"

    # The other survivor (the successor itself) re-banded the same way.
    if wait_moved "$succ" "$key" "$slot" "$succ_backup"; then
        echo "ok   - successor also redirects the band to its own backup port"
    else
        _e2e_fail "successor never re-banded to $succ_backup"
    fi

    # The replicated instances list flipped in place: victim resp -> backup.
    local instances
    instances="$(rc "$leader" RAFT GET cluster_slots_stable_instances)"
    assert_contains "instances list holds the backup addr while failed over" \
        "$succ_backup" "$instances"
    assert_not_contains "instances list dropped the dead victim's resp addr" \
        "$victim_resp" "$instances"

    # Takeover listener: reads serve its OWN store (nil), writes are -READONLY.
    local bk_reply=""
    local deadline=$((SECONDS + E2E_WAIT_TIMEOUT))
    while [ "$SECONDS" -lt "$deadline" ]; do
        bk_reply="$(bk "$succ" GET "$key" | tr -d '\r')"
        [ -z "$bk_reply" ] && break
        sleep 2
    done
    assert_eq "backup listener serves reads from its separate store (nil)" "" "$bk_reply"
    assert_contains "write on the backup listener is rejected READONLY" \
        "READONLY You can't write against a read only replica." \
        "$(bk "$succ" SET "$key" ha-failover-value | tr -d '\r')"

    # ---- Restart the corpse: Resumed observation swaps the band back ----
    local t_recover=$SECONDS
    e2e_restart_node "$victim"

    if wait_moved "$leader" "$key" "$slot" "$victim_resp"; then
        echo "ok   - survivor MOVEDs the key back to the victim's resp addr"
        assert_eq "MOVED reply names the original resp addr verbatim" \
            "MOVED $slot $victim_resp" "$LAST_MOVED"
    else
        _e2e_fail "no swap-back MOVED to $victim_resp within $((E2E_WAIT_TIMEOUT * 3))s"
    fi
    echo "info - restart -> MOVED-back-to-resp observed after $((SECONDS - t_recover))s"

    instances="$(rc "$leader" RAFT GET cluster_slots_stable_instances)"
    assert_contains "instances list restored the victim's resp addr" \
        "$victim_resp" "$instances"
    assert_not_contains "instances list dropped the backup addr after recovery" \
        "$succ_backup" "$instances"

    e2e_finish "$scenario"
}

main "$@"
