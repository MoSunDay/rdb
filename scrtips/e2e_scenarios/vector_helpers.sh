# vector_helpers.sh -- reply-parsing / owner-discovery helpers shared by
# the vector scenario (sourced AFTER env.sh: they read $E2E_HOST /
# $RDB_PORT / $VSET and call env.sh's node_resp). Pure functions, no
# cluster side effects. Content moved verbatim from
# scenario_vector_search.sh (line-count budget split).

# ---------------------------------------------------------------- CLI --
# u8 ESCAPES -> raw UTF-8 bytes on stdout (keeps this file ASCII).
u8() { printf '%b' "$1"; }

# f16_bytes v1 v2 ... -> dim*2 raw LE half-float bytes on stdout
# (FP16 is the ONLY binary vector form; struct 'e' = IEEE half).
f16_bytes() {
    python3 -c 'import struct,sys
vals = [float(a) for a in sys.argv[1:]]
sys.stdout.buffer.write(struct.pack("<%de" % len(vals), *vals))' "$@"
}

# rc CMD... -> plain (non-cluster) call to the discovered owner node.
rc() { redis-cli -h "$E2E_HOST" -p "$RDB_PORT" "$@"; }

# redis-cli reply -> one item per line. Non-TTY redis-cli prints RAW
# replies: array elements one per line (VSIM members, KNN docids),
# bare integers, bare error text -- so just drop blank lines.
rc_items() {
    printf '%s\n' "$1" | sed -e '/^[[:space:]]*$/d'
}

# FT.SEARCH reply -> hit TOTAL. Raw output prints the total count as
# the first line of the flat reply.
reply_count() {
    printf '%s\n' "$1" | sed -e '/^[[:space:]]*$/d' | sed -n '1p'
}

# member/score pair lines (TAB joined) -> score of one member.
pair_score() {
    printf '%s\n' "$1" | awk -F'\t' -v m="$2" '$1 == m { print $2; exit }'
}

# any reply -> space-joined docid sequence (formatting-agnostic, so
# redis-cli and iredis outputs are comparable).
doc_seq() {
    printf '%s\n' "$1" | grep -oE 'zh(db|se|mi)[0-9]' | tr '\n' ' '
}

# FT.SEARCH items after the integer-total header line.
reply_items() {
    rc_items "$1" | tail -n +2
}

# 0/1: is every docid in $2 from group $1 (db|se|mi)?  (docids look
# like zhdb0/zhse2/zhmi3, so the group tag is matched infix-style.)
seq_only_group() {
    local d bad=0
    for d in $2; do
        case "$d" in
        *"$1"*) ;;
        *) bad=1 ;;
        esac
    done
    printf '%s' "$bad"
}

# discover the node owning the {sem} slot: EXISTS on a foreign slot is
# refused with MOVED, on the owner it answers 0/1. --raw on purpose:
# redis-cli without a TTY prints bare `0`, never `(integer) 0`.
discover_owner() {
    local i p out
    for i in 0 1 2; do
        p=$(node_resp "$i")
        out=$(redis-cli --raw -h "$E2E_HOST" -p "$p" EXISTS "$VSET" 2>/dev/null)
        case "$out" in
        0 | 1) printf '%s' "$p"; return 0 ;;
        esac
    done
    return 1
}

