#!/usr/bin/env bash
# scenario_vector_search.sh -- real-CLI e2e for the "semantic search"
# user story: VectorSet VADD/VSIM similarity + FT.* (BM25 text, KNN
# vectors, term prefilter) + FT.INFO, driven through redis-cli 6.0.16,
# iredis 1.16.1 and the python redis-py 7.4.1 SDK against a live
# 3-node rdb cluster (env.sh harness).
#
# Every syntax/semantic claim below was verified in the source tree
# (do NOT "fix" these lines from Redis/RediSearch memory):
#   VADD key (FP16 <blob>|VALUES) dim element <vector...> -> :1 new,
#       :0 replace                    src/command/vectorset_cmd.rs:115-121
#   VSIM key [COUNT n] [WITHSCORES] [WITHATTRIBS]
#       (FP16 <blob>|VALUES <f64...>); options parse in ANY order but
#       the vector spec must be LAST (VALUES swallows the tail) and
#       takes NO leading dim token. Reply: flat array, per hit
#       member [attr] [score]; score=(cos+1)/2, i.e. cosine SIMILARITY
#       in [0,1] (not a distance), ties broken by member bytes asc.
#                                     src/command/vectorset_sim.rs:20-32,100-108
#   FP16 blob = dim * 2 RAW little-endian half floats -- there is no
#       FP32 form. Half floats of common values contain NUL bytes,
#       which argv cannot carry, so the blob is fed via `redis-cli -x`
#       (last argument from STDIN; supported by redis-cli 6.0.16).
#       src/ds/vectorset_ds.rs:238 fp16_to_f64, tests/vectorset_e2e.rs:147
#   FT.CREATE <idx> SCHEMA <f> TEXT | <f> VECTOR DIM <n> -> +OK
#                                     src/search/ft_cmd.rs:45-52
#   FT.ADD <idx> <docid> <json> -> :1 (fields = schema fields; VECTOR
#       field is a JSON number array of exactly DIM values)
#                                     src/search/ft_cmd.rs:217,172-193
#   FT.BUILD <idx> [K n] [ITERS n] [SEED n] -> +OK; NOT implicit, but
#       KNN also works pre-build (exact brute force). k is clamped to
#       #docs (src/search/ann/kmeans.rs:22), so default K=32 is safe
#       for 10 docs.                  src/search/ft_build.rs:15-18
#   FT.SEARCH <idx> <query|*> [LIMIT o n] [WITHSCORES] [NOCONTENT]
#       [NPROBE n] KNN <k> <field> (FP16 <blob>|VALUES <f64...>)
#       (the KNN block must be LAST). Reply: "<total> docid [score]
#       [content]" flat array, total = returned hits; KNN score =
#       1/(1+L2) DESC (1.0000 == exact match, %.4f format); a text
#       query + KNN = exact L2 over the prefiltered docid set; `*` +
#       KNN = SPANN probe + SQ8 rerank post-build (scores then sit
#       NEAR, not at, the exact value).
#                                     src/search/ft_search.rs:1-17,204-245
#   FT.INFO <idx> -> flat 14-element pair array: index_name, num_docs,
#       sum_doclen, avg_doclen, fields[...], ann_built, ann_centroids
#                                     src/search/ft_search.rs:247-286
#   Chinese text: index + query sides share one jieba tokenizer, so
#       real dictionary words match reliably.
#                                     src/search/tokenize.rs:1-15
#
# iredis 1.16.1: argv mode prints nothing on a non-TTY stdin, so the
# single smoke command is piped into the REPL (section H); redis-cli
# is the workhorse. Dry-call against a closed port confirmed
# FT.SEARCH/VSIM reach the network layer (connection refused, no
# client-side rejection).
#
# All keys carry the {sem} hash tag (src/hash.rs:75) => one slot, one
# owner node; the script pins RDB_PORT to that owner so plain calls
# never see MOVED. Doc titles are real Chinese words written as \xNN
# escapes through the u8() helper so this file stays pure ASCII.

source "$(dirname "${BASH_SOURCE[0]}")/env.sh"

# Reply parsing / owner discovery live in vector_helpers.sh
source "$(dirname "${BASH_SOURCE[0]}")/vector_helpers.sh"

# --------------------------------------------------------------- data --
VSET='vec:{sem}:semantic'
IDX='idx:{sem}:zh'

# Chinese vocabulary (ASCII escapes; romanization in the comments).
W_DB="$(u8 '\xe6\x95\xb0\xe6\x8d\xae\xe5\xba\x93')"                     # shujuku (group word)
W_SE="$(u8 '\xe6\x90\x9c\xe7\xb4\xa2\xe5\xbc\x95\xe6\x93\x8e')"         # sousuo yinqing (group word)
E_SUOYIN='\xe7\xb4\xa2\xe5\xbc\x95'                                     # suoyin
E_YUANLI='\xe5\x8e\x9f\xe7\x90\x86'                                     # yuanli
E_SHIWU='\xe4\xba\x8b\xe5\x8a\xa1'                                      # shiwu
E_YU='\xe4\xb8\x8e'                                                     # yu
E_SUO='\xe9\x94\x81'                                                    # suo
E_BEIFEN='\xe5\xa4\x87\xe4\xbb\xbd'                                     # beifen
E_HUIFU='\xe6\x81\xa2\xe5\xa4\x8d'                                      # huifu
E_JIAGOU='\xe6\x9e\xb6\xe6\x9e\x84'                                     # jiagou
E_JIEXI='\xe8\xa7\xa3\xe6\x9e\x90'                                      # jiexi
E_PAIXU='\xe6\x8e\x92\xe5\xba\x8f'                                      # paixu
E_SUANFA='\xe7\xae\x97\xe6\xb3\x95'                                     # suanfa
E_QUANWEN='\xe5\x85\xa8\xe6\x96\x87'                                    # quanwen
E_JIANSUO='\xe6\xa3\x80\xe7\xb4\xa2'                                    # jiansuo
E_XIAOXI='\xe6\xb6\x88\xe6\x81\xaf'                                     # xiaoxi
E_DUILIE='\xe9\x98\x9f\xe5\x88\x97'                                     # duilie
E_SHIZHAN='\xe5\xae\x9e\xe6\x88\x98'                                    # shizhan
E_BIJI='\xe7\xac\x94\xe8\xae\xb0'                                       # biji
E_RONGQI='\xe5\xae\xb9\xe5\x99\xa8'                                     # rongqi
E_BIANPAI='\xe7\xbc\x96\xe6\x8e\x92'                                    # bianpai
E_RUMEN='\xe5\x85\xa5\xe9\x97\xa8'                                      # rumen
E_ZHINAN='\xe6\x8c\x87\xe5\x8d\x97'                                     # zhinan
E_QIANDUAN='\xe5\x89\x8d\xe7\xab\xaf'                                   # qianduan
E_XINGNENG='\xe6\x80\xa7\xe8\x83\xbd'                                   # xingneng
E_YOUHUA='\xe4\xbc\x98\xe5\x8c\x96'                                     # youhua
E_SHILU='\xe5\xae\x9e\xe5\xbd\x95'                                      # shilu
E_FENSHI='\xe5\x88\x86\xe5\xb8\x83\xe5\xbc\x8f'                         # fenshi
E_YIZHIXING='\xe4\xb8\x80\xe8\x87\xb4\xe6\x80\xa7'                      # yizhixing
E_QIANTAN='\xe6\xb5\x85\xe8\xb0\x88'                                    # qiantan

# Titles: "<group word> <descriptor>". The ASCII space makes the group
# word its own jieba Han run, so index-side and query-side segmentation
# of that word CANNOT diverge (tokenize.rs runs; the same pattern is
# what lets tests/search_e2e.rs match "zhongwen jiansuo" by "jiansuo").
T_DB0="$W_DB $(u8 "$E_SUOYIN$E_YUANLI")"        # shujuku suoyin yuanli
T_DB1="$W_DB $(u8 "$E_SHIWU$E_YU$E_SUO")"       # shujuku shiwu yu suo
T_DB2="$W_DB $(u8 "$E_BEIFEN$E_YU$E_HUIFU")"    # shujuku beifen yu huifu
T_SE0="$W_SE $(u8 "$E_JIAGOU$E_JIEXI")"         # sousuo yinqing jiagou jiexi
T_SE1="$W_SE $(u8 "$E_PAIXU$E_SUANFA")"         # sousuo yinqing paixu suanfa
T_SE2="$W_SE $(u8 "$E_QUANWEN$E_JIANSUO")"      # sousuo yinqing quanwen jiansuo
T_MI0="$(u8 "$E_XIAOXI$E_DUILIE") $(u8 "$E_SHIZHAN$E_BIJI")"          # xiaoxi duilie shizhan biji
T_MI1="$(u8 "$E_RONGQI$E_BIANPAI") $(u8 "$E_RUMEN$E_ZHINAN")"         # rongqi bianpai rumen zhinan
T_MI2="$(u8 "$E_QIANDUAN") $(u8 "$E_XINGNENG$E_YOUHUA$E_SHILU")"      # qianduan xingneng youhua shilu
T_MI3="$(u8 "$E_FENSHI") $(u8 "$E_YIZHIXING$E_QIANTAN")"              # fenshi yizhixing qiantan

# docid|title|8-dim vector (fixed deterministic values, no RNG).
DOCS=(
    "zhdb0|$T_DB0|0.9,0.1,0,0,0.8,0,0,0.1"
    "zhdb1|$T_DB1|0.88,0.12,0.02,0,0.79,0,0.01,0.1"
    "zhdb2|$T_DB2|0.92,0.08,0,0.01,0.82,0.01,0,0.09"
    "zhse0|$T_SE0|0.1,0.9,0.8,0,0.1,0,0,0"
    "zhse1|$T_SE1|0.12,0.88,0.82,0.01,0.09,0,0.02,0"
    "zhse2|$T_SE2|0.09,0.91,0.78,0,0.12,0.01,0,0.01"
    "zhmi0|$T_MI0|0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5"
    "zhmi1|$T_MI1|0.25,0.75,0.3,0.6,0.4,0.2,0.7,0.35"
    "zhmi2|$T_MI2|0.7,0.2,0.1,0.9,0.3,0.6,0.15,0.8"
    "zhmi3|$T_MI3|0.33,0.66,0.44,0.22,0.55,0.11,0.77,0.88"
)

# KNN query vector == zhdb0's stored vector (exact-match top-1).
QV="0.9 0.1 0 0 0.8 0 0 0.1"

# VectorSet fixtures: two near-duplicates of A1's topic plus
# orthogonal / opposite distractors. A1=(1,0.05,0) etc.
VECS=(
    "A1|1|0.05|0"
    "A2|0.95|0.1|0"
    "A3|1.02|-0.03|0.01"
    "B|0|1|0"
    "C|0|0|1"
    "D|-1|0|0"
    "E|0.1|0.9|0.1"
    "F|0.7|0.7|0"
)

# --------------------------------------------------------------- main --
e2e_init vector_search
if ! e2e_start_cluster; then
    _e2e_fail "cluster failed to start (see harness FATAL above)"
    e2e_finish vector_search
fi

RDB_PORT="$(discover_owner)" || RDB_PORT=""
if [ -n "$RDB_PORT" ]; then
    echo "ok   - {sem} slot owner responds at port $RDB_PORT"
else
    _e2e_fail "no node owns the {sem} slot (owner discovery)"
    e2e_finish vector_search
fi

# ---- A. VectorSet: VADD 8 three-dim vectors, VSIM similarity ----------
for row in "${VECS[@]}"; do
    elem=${row%%|*}
    rest=${row#*|}
    v1=${rest%%|*}
    rest=${rest#*|}
    v2=${rest%%|*}
    v3=${rest#*|}
    got=$(rc VADD "$VSET" VALUES 3 "$elem" "$v1" "$v2" "$v3")
    assert_eq "vadd $elem -> 1" "1" "$got"
done
assert_eq "vdim reports 3" "3" "$(rc VDIM "$VSET")"
assert_eq "vcard reports 8" "8" "$(rc VCARD "$VSET")"
assert_eq "type is vectorset" "vectorset" "$(rc TYPE "$VSET")"

# VSIM full scan WITHSCORES (spec last, no dim token after VALUES).
# Self-match ranks FIRST: score=(cos+1)/2 = 1 for the queried element
# itself (tests/vectorset_e2e.rs:97-116), rank-2 is the nearest kin.
sim_all=$(rc VSIM "$VSET" WITHSCORES VALUES 1 0.05 0)
pairs=$(rc_items "$sim_all" | paste - -)
assert_eq "vsim top-1 is the queried element itself (A1)" "A1" \
    "$(printf '%s\n' "$pairs" | head -n1 | cut -f1)"
rank2=$(printf '%s\n' "$pairs" | sed -n 2p | cut -f1)
rank2_family=0
[ "$rank2" = "A2" ] && rank2_family=1
[ "$rank2" = "A3" ] && rank2_family=1
assert_eq "vsim rank-2 is a same-topic neighbor (A2/A3)" "1" "$rank2_family"
sim1=$(pair_score "$pairs" A1)
self_ok=$(awk -v s="$sim1" 'BEGIN { print (s >= 0.99999 && s <= 1.0) ? 1 : 0 }')
assert_eq "vsim A1 self-match score is 1 (f64 tolerance)" "1" "$self_ok"
range_bad=$(printf '%s\n' "$pairs" | awk -F'\t' 'NF==2 && ($2 + 0 < 0 || $2 + 0 > 1) { c++ } END { print c + 0 }')
assert_eq "vsim scores all within [0,1] (similarity, not distance)" "0" "$range_bad"
order_bad=$(awk -v a="$(pair_score "$pairs" A2)" -v d="$(pair_score "$pairs" D)" \
    'BEGIN { print (a > d) ? 0 : 1 }')
assert_eq "vsim near doc outscores opposite doc (A2 > D)" "0" "$order_bad"

sim_3=$(rc VSIM "$VSET" COUNT 3 VALUES 1 0.05 0)
assert_eq "vsim COUNT 3 truncates to 3 members" "3" "$(rc_items "$sim_3" | wc -l | tr -d ' ')"
rank2_3=$(rc_items "$sim_3" | sed -n 2p)
c3_ok=0
[ "$rank2_3" = "A2" ] && c3_ok=1
[ "$rank2_3" = "A3" ] && c3_ok=1
assert_eq "vsim COUNT 3 keeps a family member at rank 2" "1" "$c3_ok"

# Binary-form smoke: FP16 blob via redis-cli -x (last arg from STDIN).
fp16_out="$(f16_bytes 1 0.05 0 | rc -x VSIM "$VSET" COUNT 3 FP16)"
assert_eq "vsim FP16 binary query (redis-cli -x stdin) top-1 is A1" \
    "A1" "$(rc_items "$fp16_out" | sed -n 1p)"

# ---- B. FT.CREATE: title TEXT + embedding VECTOR DIM 8 ----------------
assert_contains "ft.create idx (title TEXT + embedding VECTOR DIM 8) -> OK" \
    "OK" "$(rc FT.CREATE "$IDX" SCHEMA title TEXT embedding VECTOR DIM 8)"
assert_contains "duplicate ft.create is refused" \
    "already exists" "$(rc FT.CREATE "$IDX" SCHEMA x TEXT)"

# ---- C. FT.ADD 10 Chinese docs, deterministic 8-dim vectors -----------
for row in "${DOCS[@]}"; do
    did=${row%%|*}
    rest=${row#*|}
    title=${rest%%|*}
    vec=${rest#*|}
    got=$(rc FT.ADD "$IDX" "$did" "{\"title\":\"$title\",\"embedding\":[$vec]}")
    assert_eq "ft.add $did -> 1" "1" "$got"
done

# ---- D. text search sanity, then exact KNN BEFORE FT.BUILD ------------
text_out=$(rc FT.SEARCH "$IDX" "@title:$W_DB")
assert_eq "bm25 text search on the db word hits 3 docs" "3" "$(reply_count "$text_out")"

knn_pre=$(rc FT.SEARCH "$IDX" '*' NOCONTENT WITHSCORES KNN 3 embedding VALUES $QV)
assert_eq "pre-build KNN returns 3 hits (exact brute force)" "3" "$(reply_count "$knn_pre")"
assert_eq "pre-build KNN top-1 is the queried doc itself" "zhdb0" \
    "$(reply_items "$knn_pre" | sed -n 1p)"
assert_eq "pre-build KNN top-1 score is 1.0000 (L2=0 -> 1/(1+0))" \
    "1.0000" "$(reply_items "$knn_pre" | sed -n 2p)"
assert_eq "pre-build KNN hits all come from the db group" "0" \
    "$(seq_only_group db "$(doc_seq "$knn_pre")")"

# ---- E. FT.BUILD (SPANN trainer), then probed KNN ---------------------
assert_contains "ft.build -> OK" "OK" "$(rc FT.BUILD "$IDX")"

knn_post=$(rc FT.SEARCH "$IDX" '*' NOCONTENT WITHSCORES NPROBE 8 KNN 3 embedding VALUES $QV)
assert_eq "post-build KNN returns 3 hits" "3" "$(reply_count "$knn_post")"
assert_eq "post-build KNN top-1 is still the queried doc" "zhdb0" \
    "$(reply_items "$knn_post" | sed -n 1p)"
post_s=$(reply_items "$knn_post" | sed -n 2p)
s_ok=$(awk -v s="$post_s" 'BEGIN { print (s >= 0.99 && s <= 1.0) ? 1 : 0 }')
assert_eq "post-build top-1 score in [0.99,1] (SQ8-dequantized 1/(1+L2))" "1" "$s_ok"

# ---- F. term prefilter + vector ranking -------------------------------
pre_db=$(rc FT.SEARCH "$IDX" "@title:$W_DB" NOCONTENT KNN 2 embedding VALUES $QV)
assert_eq "prefiltered KNN returns k=2 hits" "2" "$(reply_count "$pre_db")"
assert_eq "prefiltered KNN top-1 is the db query doc" "zhdb0" \
    "$(reply_items "$pre_db" | sed -n 1p)"
assert_eq "prefiltered hits only from the db group" "0" \
    "$(seq_only_group db "$(doc_seq "$pre_db")")"
assert_not_contains "prefiltered hits contain no search-engine doc" "zhse" \
    "$(doc_seq "$pre_db")"

pre_se=$(rc FT.SEARCH "$IDX" "@title:$W_SE" NOCONTENT KNN 1 embedding VALUES $QV)
assert_eq "second-prefilter KNN returns k=1 hit" "1" "$(reply_count "$pre_se")"
assert_eq "second-prefilter top-1 is a search-engine doc" "0" \
    "$(seq_only_group se "$(doc_seq "$pre_se")")"

# ---- G. FT.INFO -------------------------------------------------------
info=$(rc FT.INFO "$IDX")
assert_contains "ft.info echoes index_name" "$IDX" "$info"
n_docs=$(printf '%s\n' "$info" | grep -A1 '^num_docs$' | tail -n1)
assert_eq "ft.info num_docs == 10" "10" "$n_docs"
assert_contains "ft.info schema lists the VECTOR field" 'VECTOR' "$info"
dim_line=$(printf '%s\n' "$info" | grep -A2 '^embedding$' | tail -n1)
assert_contains "ft.info embedding dim is 8" "8" "$dim_line"
assert_eq "ft.info ann_built == 1 after FT.BUILD" "1" \
    "$(printf '%s\n' "$info" | grep -A1 '^ann_built$' | tail -n1)"

# ---- H. iredis smoke: same KNN query, same answer ---------------------
# iredis 1.16.1: argv mode is silent when stdin is not a TTY, so the
# single command is fed through the REPL via stdin (its reply layout
# for this query matches redis-cli --raw: total, docid, score, ...).
ir_out="$(printf "FT.SEARCH %s '*' NOCONTENT WITHSCORES NPROBE 8 KNN 3 embedding VALUES %s\n" \
        "$IDX" "$QV" \
    | timeout 60 iredis -h "$E2E_HOST" -p "$RDB_PORT" -a "$RDB_E2E_TOKEN" 2>/dev/null)"
ir_docs=$(doc_seq "$ir_out")
ir_nonempty=0
[ -n "$ir_docs" ] && ir_nonempty=1
assert_eq "iredis KNN reply non-empty" "1" "$ir_nonempty"
assert_eq "iredis KNN docids identical to redis-cli" "$(doc_seq "$knn_post")" "$ir_docs"


# ---- I. python SDK (redis-py): strict-RESP wire contract ---------------
# redis-py 7.4.1 is the third real client. Its parser enforces strict
# reply TYPING, which is exactly the FT.SEARCH wire shape fixed in this
# suite: the reply must arrive as a flat array whose element 0 is an
# integer total, followed by docid/score bulk-string pairs. Only the
# direct Redis() client is exercised: RedisCluster() additionally runs
# COMMAND during its handshake (not implemented server-side), while the
# topology call CLUSTER SLOTS already returns the expected slot ranges.
# A transient socket blip inside the SDK script aborts it mid-way and
# every later py_val assert reads "" (one blip -> 6 red asserts); retry
# the whole SDK probe when the final "docs" line is missing.
_py_sdk="$E2E_WORKDIR/py_sdk.py"
cat > "$_py_sdk" <<'PYEOF'
import os
import redis

host, port = os.environ["PY_HOST"], int(os.environ["PY_PORT"])
token, idx = os.environ["PY_TOKEN"], os.environ["PY_IDX"]
qv = os.environ["PY_QV"].split()
r = redis.Redis(host=host, port=port, password=token, protocol=2,
                socket_timeout=10, socket_connect_timeout=5)
print("ping", 1 if r.ping() else 0)

# topology: CLUSTER SLOTS ranges must tile [0, 16383] exactly once
ranges = sorted((lo, hi) for lo, hi, *_ in r.execute_command("CLUSTER", "SLOTS"))
tiled = (ranges[0][0] == 0 and ranges[-1][1] == 16383
         and all(h + 1 == lo for (_, h), (lo, _) in zip(ranges, ranges[1:])))
print("slots_tiled", 1 if tiled else 0)

# plain roundtrip: raw bytes preserved both ways
key, val = "{sem}:sdk", b"\xe5\xbc\x95\xe6\x93\x8e value"
print("roundtrip", 1 if r.set(key, val) and r.get(key) == val else 0)
r.delete(key)

# FT.SEARCH flat-array contract (NOCONTENT WITHSCORES)
res = r.execute_command("FT.SEARCH", idx, "*", "NOCONTENT", "WITHSCORES",
                        "NPROBE", "8", "KNN", "3", "embedding", "VALUES", *qv)
ok = (isinstance(res, list) and len(res) >= 1 and isinstance(res[0], int)
      and len(res) - 1 == 2 * res[0]
      and all(isinstance(x, bytes) for x in res[1:]))
print("flat_shape", 1 if ok else 0)
if ok:
    print("total", res[0])
    print("docs", b" ".join(res[1:-1:2]).decode())
PYEOF
py_out=""
for _try in 1 2 3; do
    py_out="$(PY_HOST="$E2E_HOST" PY_PORT="$RDB_PORT" PY_TOKEN="$RDB_E2E_TOKEN" \
        PY_IDX="$IDX" PY_QV="$QV" python3 "$_py_sdk" 2>&1)"
    [ -n "$(printf '%s\n' "$py_out" | sed -n 's/^docs //p')" ] && break
    echo "-- python SDK attempt $_try incomplete; retrying --"
    sleep 2
done
py_val() { printf '%s\n' "$py_out" | sed -n "s/^$1 //p" | head -n1; }
assert_eq "redis-py PING over AUTH answers True" "1" "$(py_val ping)"
assert_eq "redis-py SET/GET roundtrip preserves raw bytes" "1" "$(py_val roundtrip)"
assert_eq "CLUSTER SLOTS ranges tile 0..16383 exactly" "1" "$(py_val slots_tiled)"
assert_eq "FT.SEARCH reply parses as flat array (int total + byte pairs)" "1" \
    "$(py_val flat_shape)"
assert_eq "redis-py KNN total matches redis-cli" \
    "$(reply_count "$knn_post")" "$(py_val total)"
assert_eq "redis-py KNN docids identical to redis-cli" \
    "$(doc_seq "$knn_post" | xargs)" "$(py_val docs | xargs)"

e2e_finish vector_search