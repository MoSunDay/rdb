#!/usr/bin/env bash
# scenario_migrate.sh - e2e: slot migration driven by REAL redis-cli
# against the 3-node rdb cluster assembled by env.sh -- the CLI-suite
# twin of tests/migrate_e2e.rs. Steps: (a) hash-tag family + band owner
# discovery, (b) seed 2 strings + a hash on the OWNER (retrying through
# the post-init settle), (c) `migrate task` on the leader -> done/3 in
# `migrate list`, (d) dst serves, src + bystander MOVED to dst,
# GETKEYSINSLOT flips, (e) the reverse task proves the busy guard was
# released and the family returns home.
#
# Semantics verified against source/tests BEFORE writing this script:
# - MIGRATE subcommands are LOWERCASE-ONLY (src/command/migrate/mod.rs:31
#   -40 + its test): argv[1] "TASK" falls through to the DATA command and
#   answers "ERR wrong number of arguments for 'migrate' command". So the
#   orchestration is `migrate task <slot> <src-resp> <dst-resp>` and the
#   record is `migrate list` -- exactly the casing tests/migrate_e2e.rs
#   :141-149,185-193 uses.
# - `migrate task` drives the whole reshard protocol SYNCHRONOUSLY (src
#   /command/migrate/task.rs:52-80: src MIGRATING -> dst IMPORTING ->
#   GETKEYSINSLOT drain -> NODE on both -> STABLE) and only then replies
#   +OK and persists the JSON record, so `migrate list` needs no poll for
#   the status itself (still polled once below for shape, not timing).
# - The task record is ONE bulk JSON line:
#   {"slot":N,"src":"h:p","dst":"h:p","status":"done","moved":K}
#   (task.rs:task_json). migrate list is raft-replicated (key
#   `migrate_task`) and the Rust test reads it on the leader.
# - Busy guard: one run at a time per process (task.rs:64-68
#   migrate_busy.swap) -- a successful run MUST clear it, which the
#   reverse task below exercises (migrate_e2e.rs:226-243).
# - Bands after CLUSTER INIT (3 equal splits, src/command/cluster.rs +
#   tests/migrate_e2e.rs:53-59): slot<=5461 -> resp_0, <=10922 -> resp_1,
#   else resp_2. MOVED text is "MOVED <slot> <addr>"
#   (src/router.rs:74-77) and the addr is the owner's RESP bind.
# - The slot's NEW owner reaches the SOURCE node immediately (it ran the
#   NODE hop itself) but a BYSTANDER only learns the raft owner map
#   through the 3s topology ticker, so bystander MOVED convergence is
#   POLLED for up to 30s (migrate_e2e.rs:206-216 waits the same way).
# - Seeding may answer MOVED for the first seconds after e2e_start_cluster
#   (band assignment + instance registration replicate round-trip), so
#   every seed write is retried for up to 30s -- mirroring the Rust
#   retry_reply helper (migrate_e2e.rs:61-75).

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

addr () { echo "$E2E_HOST:$(node_resp "$1")"; } # RESP bind of node <idx>

# Retry a command on node <idx> until its whole reply equals <want>.
# Long poll: up to 30 tries x 1s (post-init MOVED settle window).
until_reply () { # tries idx want args...
    local tries=$1 idx=$2 want=$3 out i
    shift 3
    for i in $(seq 1 "$tries"); do
        out="$(rc "$idx" "$@")"
        [ "$out" = "$want" ] && { echo 0; return 0; }
        sleep 1
    done
    echo 1
    return 1
}

# Poll a command on node <idx> until its reply CONTAINS <needle>.
# Long poll: up to <tries> x 1s (raft owner-map replication ticker).
poll_contains () { # tries idx needle desc args...
    local tries=$1 idx=$2 needle=$3 desc=$4 out i
    shift 4
    for i in $(seq 1 "$tries"); do
        out="$(rc "$idx" "$@")"
        case "$out" in
        *"$needle"*) echo "ok   - $desc"; return 0 ;;
        esac
        sleep 1
    done
    echo "FAIL: $desc: [$needle] not seen within ${tries}s (last: [$out])" >&2
    E2E_FAILS=$((E2E_FAILS + 1))
    return 1
}

# Number of non-empty lines of a reply (GETKEYSINSLOT key count).
key_count () { printf '%s\n' "$1" | grep -c . || true; }

main () {
    local scenario=migrate
    e2e_init "$scenario"
    e2e_start_cluster >/dev/null || { echo "FATAL: cluster bring-up failed" >&2; exit 1; }
    local leader
    leader=$(e2e_find_leader) || { echo "FATAL: no leader" >&2; exit 1; }

    # ---- (a) hash-tag family -> slot -> band owner ----------------------
    local slot src dst by
    slot="$(rc "$leader" CLUSTER KEYSLOT '{mig}k1')"
    case "$slot" in
    ''|*[!0-9]*) assert_eq "CLUSTER KEYSLOT replies an integer" "int" "$slot" ;;
    *) echo "ok   - CLUSTER KEYSLOT replies an integer ($slot)" ;;
    esac
    if [ "$slot" -le 5461 ]; then src=0
    elif [ "$slot" -le 10922 ]; then src=1
    else src=2
    fi
    dst=$(((src + 1) % 3))
    by=$(((src + 2) % 3))
    echo "slot $slot: src=n$src dst=n$dst bystander=n$by leader=n$leader"

    # ---- (b) seed on the OWNER, retrying the post-init settle -----------
    assert_eq "SET {mig}s1 lands on the band owner (retry through MOVED)" 0 \
        "$(until_reply 30 "$src" OK SET '{mig}s1' v1)"
    assert_eq "SET {mig}s2 lands on the band owner" 0 \
        "$(until_reply 30 "$src" OK SET '{mig}s2' v2)"
    assert_eq "HSET {mig}h f v lands on the band owner" 0 \
        "$(until_reply 30 "$src" 1 HSET '{mig}h' f v)"
    local seeded
    seeded="$(rc "$src" CLUSTER GETKEYSINSLOT "$slot" 100)"
    assert_eq "the owner lists all 3 keys of the slot" "3" \
        "$(key_count "$seeded")"
    assert_contains "GETKEYSINSLOT lists the hash" "{mig}h" "$seeded"
    assert_contains "GETKEYSINSLOT lists s1" "{mig}s1" "$seeded"
    assert_contains "GETKEYSINSLOT lists s2" "{mig}s2" "$seeded"
    assert_eq "GET on the owner serves the value" "v1" \
        "$(rc "$src" GET '{mig}s1')"

    # ---- (c) orchestrate the migration on the leader --------------------
    assert_contains "MIGRATE subcommands are lowercase-only: TASK is" \
        "ERR wrong number of arguments for 'migrate' command" \
        "$(rc "$leader" MIGRATE TASK "$slot" "$(addr "$src")" "$(addr "$dst")")"
    local t0 reply
    t0=$SECONDS
    reply="$(rc "$leader" migrate task "$slot" "$(addr "$src")" "$(addr "$dst")")"
    echo "migrate task n$src -> n$dst took $((SECONDS - t0))s"
    assert_eq "migrate task replies OK" "OK" "$reply"
    poll_contains 10 "$leader" '"status":"done"' \
        "migrate list reports the task done" migrate list
    poll_contains 5 "$leader" '"moved":3' \
        "migrate list reports all 3 keys moved" migrate list
    assert_contains "migrate list names the slot" "\"slot\":$slot" \
        "$(rc "$leader" migrate list)"

    # ---- (d) data on dst, MOVED everywhere else, keys flipped -----------
    assert_eq "the destination serves s1" "v1" "$(rc "$dst" GET '{mig}s1')"
    assert_eq "the destination serves s2" "v2" "$(rc "$dst" GET '{mig}s2')"
    assert_contains "the hash moved intact" "v" \
        "$(rc "$dst" HGETALL '{mig}h')"
    assert_eq "the source MOVEDs to the dst addr" \
        "MOVED $slot $(addr "$dst")" "$(rc "$src" GET '{mig}s1')"
    poll_contains 30 "$by" "MOVED $slot $(addr "$dst")" \
        "the bystander MOVEDs to dst once the owner map replicates" \
        GET '{mig}s1'
    assert_eq "GETKEYSINSLOT is empty on the source" "0" \
        "$(key_count "$(rc "$src" CLUSTER GETKEYSINSLOT "$slot" 100)")"
    assert_eq "GETKEYSINSLOT holds 3 keys on the destination" "3" \
        "$(key_count "$(rc "$dst" CLUSTER GETKEYSINSLOT "$slot" 100)")"

    # ---- (e) reverse: the busy guard must have been released ------------
    t0=$SECONDS
    reply="$(rc "$leader" migrate task "$slot" "$(addr "$dst")" "$(addr "$src")")"
    echo "reverse migrate task took $((SECONDS - t0))s"
    assert_eq "the reverse migrate task is accepted (no BUSY)" "OK" "$reply"
    assert_eq "the family returned home: src serves s1 again" "v1" \
        "$(rc "$src" GET '{mig}s1')"
    assert_eq "the former dst MOVEDs the slot home" \
        "MOVED $slot $(addr "$src")" "$(rc "$dst" GET '{mig}s2')"
    assert_eq "GETKEYSINSLOT is back to 3 keys on the source" "3" \
        "$(key_count "$(rc "$src" CLUSTER GETKEYSINSLOT "$slot" 100)")"
    assert_contains "migrate list reflects the reverse task" "\"src\":\"$(addr "$dst")\"" \
        "$(rc "$leader" migrate list)"

    e2e_finish "$scenario"
}

main "$@"
