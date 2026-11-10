//! FT.SEARCH option parsing: the query blob lowers to the shared
//! `query_tree::QueryNode` (v2 dialect: implicit AND, `@field:term`
//! scopes, `"quoted phrases"`, trailing-`*` prefix, `|`/OR, `-`/NOT;
//! `*` alone = match-all) and the option tail (`LIMIT o n`,
//! `WITHSCORES`, `NOCONTENT`, `NPROBE`, and vector search
//! `KNN <k> @<field>` + `FP16 <blob>` / `VALUES <f64...>` tail), plus
//! `SORTBY <field> [ASC|DESC]` (NUMERIC doc-value sort; rejected
//! beside KNN) and `RETURN <n> <f...>` (reply projection), plus
//! the KNN query-vector decode (FP16 or VALUES -> f64, deferred until
//! the schema's dim is known). Execution lives in `ft_search` /
//! `eval_tree`.

use crate::command::hash_cmd::parse_f64;
use crate::command::zset_util::eq_ignore_case;
use crate::command::Ctx;
use crate::ds::vectorset_ds;

use super::query_tree::{parse_query_tree, QueryDialect, QueryNode};

const DEFAULT_LIMIT: usize = 10;

#[derive(Debug, Default)]
pub struct SearchOpts {
    pub offset: usize,
    pub count: usize,
    pub with_scores: bool,
    pub no_content: bool,
    /// `SORTBY <field> [ASC|DESC]`: (field, is_desc); the sort itself
    /// runs in `ft_search` over the NUMERIC doc-values.
    pub sortby: Option<(Vec<u8>, bool)>,
    /// `RETURN <n> <f...>`: `None` = option unused (doc as stored),
    /// `Some(list)` = keep exactly these keys (`Some([])` for
    /// `RETURN 0` -> `{}`).
    pub ret: Option<Vec<Vec<u8>>>,
    /// `HIGHLIGHT ...`: `None` = option unused.
    pub highlight: Option<HighlightOpts>,
    pub knn: Option<KnnOpts>,
}

/// `HIGHLIGHT [FIELDS <n> <f...>] [TAGS <open> <close>]`: wrap the
/// matched query terms of the reply docs in tags. `fields: None` =
/// every TEXT field of the schema (expanded in `ft_search` once the
/// meta is known); tags default to `<b>`/`</b>`. Repeats override
/// (last wins, K1/B convention); NOT usable with NOCONTENT.
#[derive(Debug, Clone)]
pub struct HighlightOpts {
    pub fields: Option<Vec<Vec<u8>>>,
    pub open: Vec<u8>,
    pub close: Vec<u8>,
}

impl Default for HighlightOpts {
    fn default() -> Self {
        HighlightOpts {
            fields: None,
            open: b"<b>".to_vec(),
            close: b"</b>".to_vec(),
        }
    }
}

/// KNN as far as arg parsing can go; the vector body decodes once the
/// meta's dim is known.
#[derive(Debug, Clone)]
pub struct KnnOpts {
    pub k: usize,
    pub field: Vec<u8>,
    pub nprobe: usize,
    pub blob: Vec<u8>,
    pub values: Vec<Vec<u8>>,
}

impl Default for KnnOpts {
    fn default() -> Self {
        KnnOpts {
            k: 10,
            field: Vec::new(),
            nprobe: 4,
            blob: Vec::new(),
            values: Vec::new(),
        }
    }
}

/// Options after `<index> <query>` plus the parsed query tree.
pub(super) fn parse_opts(ctx: &Ctx<'_>) -> Result<(Option<QueryNode>, SearchOpts), &'static str> {
    if ctx.args.len() < 2 {
        return Err("ERR wrong number of arguments for 'ft.search' command");
    }
    let terms = parse_query_tree(&ctx.args[1], QueryDialect::Ft)?;
    let mut opts = SearchOpts {
        offset: 0,
        count: DEFAULT_LIMIT,
        ..Default::default()
    };
    let mut i = 2;
    while i < ctx.args.len() {
        let arg = ctx.args[i].as_slice();
        let u64_at = |j: usize| -> Option<u64> {
            ctx.args
                .get(j)
                .and_then(|a| std::str::from_utf8(a).ok())
                .and_then(|t| t.parse::<u64>().ok())
        };
        if eq_ignore_case(arg, b"LIMIT") {
            let (Some(o), Some(c)) = (u64_at(i + 1), u64_at(i + 2)) else {
                return Err("ERR bad LIMIT");
            };
            opts.offset = o.min(1_000_000) as usize;
            opts.count = c.min(1_000_000) as usize;
            i += 3;
        } else if eq_ignore_case(arg, b"WITHSCORES") {
            opts.with_scores = true;
            i += 1;
        } else if eq_ignore_case(arg, b"NOCONTENT") {
            if opts.highlight.is_some() {
                return Err("ERR HIGHLIGHT cannot be used with NOCONTENT");
            }
            opts.no_content = true;
            i += 1;
        } else if eq_ignore_case(arg, b"NPROBE") {
            let Some(n) = u64_at(i + 1) else {
                return Err("ERR bad NPROBE");
            };
            opts.knn.get_or_insert_with(KnnOpts::default).nprobe = n.clamp(1, 4096) as usize;
            i += 2;
        } else if eq_ignore_case(arg, b"SORTBY") {
            if opts.knn.is_some() {
                return Err("ERR SORTBY cannot be used with KNN");
            }
            let Some(f) = ctx.args.get(i + 1) else {
                return Err("ERR bad SORTBY");
            };
            let mut f = f.clone();
            if f.first() == Some(&b'@') {
                f.remove(0);
            }
            // 方向词可省（默认 ASC）；出现时必须是 ASC/DESC——一个
            // 既非方向词也非已知选项的尾随词按 bad SORTBY 报错
            let mut desc = false;
            let mut j = i + 2;
            if let Some(dir) = ctx.args.get(j) {
                if eq_ignore_case(dir, b"DESC") {
                    desc = true;
                    j += 1;
                } else if eq_ignore_case(dir, b"ASC") {
                    j += 1;
                } else if !is_option_kw(dir) {
                    return Err("ERR bad SORTBY");
                }
            }
            opts.sortby = Some((f, desc));
            i = j;
        } else if eq_ignore_case(arg, b"RETURN") {
            let Some(n) = u64_at(i + 1) else {
                return Err("ERR bad RETURN");
            };
            if n as usize > ctx.args.len() - (i + 2) {
                return Err("ERR bad RETURN");
            }
            opts.ret = Some(ctx.args[i + 2..i + 2 + n as usize].to_vec());
            i += 2 + n as usize;
        } else if eq_ignore_case(arg, b"KNN") {
            if opts.sortby.is_some() {
                return Err("ERR SORTBY cannot be used with KNN");
            }
            let (Some(k), Some(field)) = (u64_at(i + 1), ctx.args.get(i + 2)) else {
                return Err("ERR bad KNN");
            };
            let mut field = field.clone();
            if field.first() == Some(&b'@') {
                field.remove(0);
            }
            let mut knn = KnnOpts {
                k: k.clamp(1, 10_000) as usize,
                field,
                nprobe: opts.knn.as_ref().map_or(4, |k| k.nprobe),
                ..Default::default()
            };
            i += 3;
            let next = ctx.args.get(i).map_or(&[][..], |v| v.as_slice());
            if eq_ignore_case(next, b"FP16") {
                let Some(blob) = ctx.args.get(i + 1) else {
                    return Err("ERR FP16 needs a blob");
                };
                knn.blob = blob.clone();
                i += 2;
            } else if eq_ignore_case(next, b"VALUES") {
                knn.values = ctx.args[i + 1..].to_vec();
                i = ctx.args.len();
            } else {
                return Err("ERR KNN needs FP16 or VALUES");
            }
            opts.knn = Some(knn);
        } else if eq_ignore_case(arg, b"HIGHLIGHT") {
            if opts.no_content {
                return Err("ERR HIGHLIGHT cannot be used with NOCONTENT");
            }
            let mut hl = HighlightOpts::default();
            i += 1;
            // FIELDS/TAGS sub-options: any order, each at most once,
            // repeats override; anything else ends the sub-tail.
            while i < ctx.args.len() {
                let sub = ctx.args[i].as_slice();
                if eq_ignore_case(sub, b"FIELDS") {
                    let Some(n) = u64_at(i + 1) else {
                        return Err("ERR bad HIGHLIGHT");
                    };
                    if n as usize > ctx.args.len() - (i + 2) {
                        return Err("ERR bad HIGHLIGHT");
                    }
                    hl.fields = Some(ctx.args[i + 2..i + 2 + n as usize].to_vec());
                    i += 2 + n as usize;
                } else if eq_ignore_case(sub, b"TAGS") {
                    let (Some(o), Some(c)) = (ctx.args.get(i + 1), ctx.args.get(i + 2)) else {
                        return Err("ERR bad HIGHLIGHT");
                    };
                    hl.open = o.clone();
                    hl.close = c.clone();
                    i += 3;
                } else {
                    break;
                }
            }
            opts.highlight = Some(hl);
        } else {
            return Err("ERR unknown ft.search option");
        }
    }
    Ok((terms, opts))
}

/// A trailing token that is neither a direction word nor a known
/// option keyword after `SORTBY <field>` means a malformed SORTBY.
const OPTION_KWS: [&[u8]; 8] = [
    b"LIMIT",
    b"WITHSCORES",
    b"NOCONTENT",
    b"NPROBE",
    b"KNN",
    b"SORTBY",
    b"RETURN",
    b"HIGHLIGHT",
];

fn is_option_kw(arg: &[u8]) -> bool {
    OPTION_KWS.iter().any(|k| eq_ignore_case(arg, k))
}

/// The KNN query vector, decoded once dim is known.
pub(super) fn knn_vector(knn: &KnnOpts, dim: u64) -> Result<Vec<f64>, &'static str> {
    if !knn.blob.is_empty() {
        if knn.blob.len() != dim as usize * 2 {
            return Err("ERR invalid FP16 vector");
        }
        return Ok(knn
            .blob
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| vectorset_ds::fp16_to_f64(u16::from_le_bytes(*c)))
            .collect());
    }
    if knn.values.len() != dim as usize {
        return Err("ERR invalid vector value");
    }
    knn.values
        .iter()
        .map(|a| parse_f64(a).ok_or("ERR invalid vector value"))
        .collect()
}
