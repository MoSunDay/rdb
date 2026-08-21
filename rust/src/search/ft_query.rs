//! FT.SEARCH query parsing: the v1 dialect (`*` = match-all,
//! whitespace-separated AND terms, optional `@field:term` scopes)
//! tokenized with the SAME tokenizer as indexing, the option tail
//! (`LIMIT o n`, `WITHSCORES`, `NOCONTENT`, `NPROBE`, and vector
//! search `KNN <k> @<field>` + `FP16 <blob>` / `VALUES <f64...>`
//! tail), and the KNN query-vector decode (FP16 or VALUES -> f64,
//! deferred until the schema's dim is known). Execution lives in
//! `ft_search`.

use crate::command::hash_cmd::parse_f64;
use crate::command::zset_util::eq_ignore_case;
use crate::command::Ctx;
use crate::ds::vectorset_ds;

use super::tokenize::tokenize;

const DEFAULT_LIMIT: usize = 10;

/// One parsed query term: the field scope (None = every TEXT field)
/// and its tokenized sub-terms (CJK queries yield several -- all
/// AND-ed, like several bare terms).
#[derive(Debug, PartialEq)]
pub struct QueryTerm {
    pub field: Option<Vec<u8>>,
    pub terms: Vec<Vec<u8>>,
}

#[derive(Debug, Default)]
pub struct SearchOpts {
    pub offset: usize,
    pub count: usize,
    pub with_scores: bool,
    pub no_content: bool,
    pub knn: Option<KnnOpts>,
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

/// Split the query blob; `None` = match-all `*`.
fn parse_query(query: &[u8]) -> Result<Option<Vec<QueryTerm>>, &'static str> {
    let text = std::str::from_utf8(query).map_err(|_| "ERR query must be UTF-8")?;
    if text.trim() == "*" {
        return Ok(None);
    }
    let mut out = Vec::new();
    for word in text.split_whitespace() {
        let (field, term) = match word.split_once(':') {
            Some((f, t)) if f.starts_with('@') => (Some(f.as_bytes()[1..].to_vec()), t),
            _ => (None, word),
        };
        let toks = tokenize(term);
        if toks.is_empty() {
            continue;
        }
        out.push(QueryTerm {
            field,
            terms: toks.into_iter().map(|t| t.into_bytes()).collect(),
        });
    }
    Ok(Some(out))
}

/// Options after `<index> <query>` plus the parsed query terms.
pub(super) fn parse_opts(
    ctx: &Ctx<'_>,
) -> Result<(Option<Vec<QueryTerm>>, SearchOpts), &'static str> {
    if ctx.args.len() < 2 {
        return Err("ERR wrong number of arguments for 'ft.search' command");
    }
    let terms = parse_query(&ctx.args[1])?;
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
            opts.no_content = true;
            i += 1;
        } else if eq_ignore_case(arg, b"NPROBE") {
            let Some(n) = u64_at(i + 1) else {
                return Err("ERR bad NPROBE");
            };
            opts.knn.get_or_insert_with(KnnOpts::default).nprobe = n.clamp(1, 4096) as usize;
            i += 2;
        } else if eq_ignore_case(arg, b"KNN") {
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
        } else {
            return Err("ERR unknown ft.search option");
        }
    }
    Ok((terms, opts))
}

/// The KNN query vector, decoded once dim is known.
pub(super) fn knn_vector(knn: &KnnOpts, dim: u64) -> Result<Vec<f64>, &'static str> {
    if !knn.blob.is_empty() {
        if knn.blob.len() != dim as usize * 2 {
            return Err("ERR invalid FP16 vector");
        }
        return Ok(knn
            .blob
            .chunks_exact(2)
            .map(|c| vectorset_ds::fp16_to_f64(u16::from_le_bytes([c[0], c[1]])))
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
