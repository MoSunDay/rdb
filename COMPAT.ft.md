# COMPAT.ft.md — Full-text + vector search contract & deviation ledger (Rust-only)

Split out of `COMPAT.md` (2026-11-10, wukong-ft Batch 2 + P3 highlight sync)
when the section was about to outgrow that file's 800-line size budget;
conventions unchanged. The Go archive has no search engine, so every item here
is a Rust-vs-**RediSearch** (FT.*) / ES-frontend contract. Program record:
`plans/2026-11-09-wukong-ft/` + `features/changelog/2026-11-09/wukong-ft-*`.

## Full-text + vector search (FT.*, Rust-only)

The Go archive has no search engine; the Rust tree ships one on the typed-record
data plane (kinds `0x13`-`0x1A`, one physical family, so index-key TTL purges
docs, postings, term stats, centroids and ANN partitions together).

- Commands: `FT.CREATE <idx> [STOPWORDS <n> <w...>] [K1 <f>] [B <f>] SCHEMA <f>
  TEXT | KEYWORD | NUMERIC | <f> VECTOR DIM <n>` (options precede SCHEMA, any
  order, case-insensitive; at most one VECTOR field), `FT.ADD <idx> <docid>
  <json>` (add/replace, one fsync), `FT.DEL <idx> <docid>`, `FT.DROP`/
  `FT.dropindex`, `FT.INFO`, `FT.BUILD [K n] [ITERS n] [SEED n]`,
  `FT.SEARCH <idx> <query> [LIMIT o c] [WITHSCORES] [NOCONTENT]
  [SORTBY <f> [ASC|DESC]] [RETURN <n> <f...>] [HIGHLIGHT [FIELDS <n> <f...>]
  [TAGS <open> <close>]] [NPROBE n] [KNN k <field> FP16 <blob>|VALUES v...]`.
- Tokenizer (index and query sides are identical by construction): Latin
  alphanumeric runs lowercased; Han runs cut by jieba (embedded dict + HMM),
  dictionary misses fall back to overlapping bigrams (the CJKAnalyzer trick);
  an isolated single character emits itself.
- Stopwords (2026-11-10): an index-level list via `FT.CREATE ... STOPWORDS`,
  EMPTY by default (RediSearch ships a 174-word English list — deviation).
  The index side drops stopword tokens BEFORE position assignment, so the
  survivors' positions COMPACT (RediSearch keeps holes — deviation: "the quick
  the fox" with "the" stopped makes phrase `"quick fox"` match). The query
  side filters stopword terms inside term/phrase evaluation (a stopword-only
  term or phrase matches nothing; `term`/`terms`/`range` are NOT filtered);
  ES-created indexes always carry the empty default.
- Postings v2 (`0x1A`, 2026-11-09): entries carry strictly-increasing token
  ordinals (delta-LEB128); writes always emit v2 and delete any v1 (`0x15`)
  twin on touch, so migration is lazy-by-rewrite. Old binaries can still read
  v1 records (dual read), but an OLD binary reading a NEW store sees 0x1A as an
  unallocated kind — deploy readers (and only then writers) first when mixing
  versions. Phrase queries on position-less v1 entries degrade to plain AND —
  documented deviation.
- Query dialect v2 (`FT.SEARCH` and the ES `query_string` share ONE parser):
  whitespace clauses are a STRICT implicit AND; `@field:` scopes; `"quoted
  phrases"` need adjacent positions (single-token phrases fold to a Term);
  trailing `*` on a single-token stem is a prefix scan over the posting-key
  range (a multi-token CJK stem degrades to AND of its tokens, star dropped —
  deviation); `|`/`OR` union, `AND` explicit, `NOT`/leading `-` complement
  (case-sensitive keywords; lowercase stays a term); `*` alone matches all;
  no parentheses. A clause scoped to a KEYWORD field matches EXACT bytes
  as typed (keyword postings are un-tokenized -- ES `term`/`match` parity):
  `@kw:Foo` matches only the value `Foo`, a quoted keyword body matches the
  whole multi-word value, and the wrong case matches nothing (pre-2026-11-10
  the shared grammar lowercased the clause, making mixed-case keyword values
  silently unreachable). `@num:[min max]` is an INCLUSIVE NUMERIC range over the
  columnar doc-values in BOTH dialects (bounds are floats or `*`/`±inf` for
  open ends; scoped syntax only, a bare `[1 5]` stays a word; exclusive `(`
  bounds, fuzzy and geometric filters are NOT supported; a malformed range is
  a query error). A range on a non-NUMERIC field matches nothing silently;
  range hits score a constant 1.0 (numeric is not BM25-ranked, same as the ES
  front). NOTE this is a behavior fix, not a change of contract: the
  v1 docs already said terms are AND-ed, but the v1 code unioned them.
- Text ranking: BM25 (Lucene idf `ln(1+(n-df+0.5)/(df+0.5))`) with per-index
  K1/B knobs from FT.CREATE (defaults 1.2/0.75), terms AND-ed within a query,
  ties broken by docid ascending.
- Vectors: SQ8 quantization (per-dimension min/scale, field-global calibration)
  plus k-means centroids in a SPANN-style partition layout. `FT.BUILD` retrains
  and repartitions; KNN probes `nprobe` nearest partitions and reranks on
  dequantized vectors, score = `1/(1+L2)`. Without a trained table KNN
  degrades to exact brute force. A `@field:term` query before `KNN`
  prefilters candidates by text match (exact vectors, no SQ8 error).
- SORTBY/RETURN (2026-11-10): `SORTBY <f> [ASC|DESC]` sorts the full match set
  by a NUMERIC field's doc-value (missing values last in BOTH directions,
  docid-ascending tiebreak) before LIMIT; it is mutually exclusive with KNN
  (either order) and errors on a non-NUMERIC field. WITHSCORES still reports
  the relevance scores. `RETURN <n> <f...>` projects the stored JSON to the
  listed keys (absent keys skipped; `RETURN 0` yields `{}`); the
  re-serialized key order may differ from the stored document — deviation.
- HIGHLIGHT (2026-11-10, P3 pool graduate): wraps matched tokens of the
  returned JSON in `TAGS` (default `<b>` `</b>`) — grammar
  `HIGHLIGHT [FIELDS n f...] [TAGS open close]`, repeatable sub-options after
  one HIGHLIGHT, repeated HIGHLIGHT last-wins. Scope: TEXT fields only, string
  JSON values only (a numeric value stored under a TEXT field is never
  wrapped); FIELDS defaults to every TEXT field, unknown listed fields are
  silently skipped. Matched = query-tree Term/Phrase tokens (exact,
  case-insensitive via the tokenizer) and Prefix stems (token starts_with);
  stopword terms and `Not` subtrees are NOT wrapped; numeric ranges highlight
  nothing. Interplay: after RETURN projection (a projected-out field cannot
  be highlighted), beside KNN/SORTBY/LIMIT/WITHSCORES; rejected with NOCONTENT
  (either order). SUMMARIZE is NOT implemented; the ES front's `highlight`
  request key is tolerated and ignored (existing unknown-key rule —
  deviations). Re-serialization caveat: like RETURN, key order of the
  re-serialized doc may differ from the stored bytes.
- Cross-node: an index lives on the slot of its key; non-local requests get
  `MOVED` like any other command. Cross-index queries are client fan-out —
  there is no distributed query planner. `FT.SEARCH` replies are node-local:
  `:N` then per hit docid [, score] [, original JSON].

