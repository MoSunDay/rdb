//! VSIM: brute-force similarity search over one vector set. Every elem
//! record is decoded and scored with cosine similarity -- O(n*dim), no
//! HNSW graph is maintained (documented deviation, see COMPAT.md; that
//! also makes EF/EXPLORE/FILTER unknown options). score = (cos+1)/2 in
//! [0,1], ties broken by element byte order (ascending).

use crate::command::hash_cmd::{arity, WRONGTYPE};
use crate::command::vectorset_cmd::{
    parse_vector, vectorset_state, VectorSetState, ERR_COUNT, ERR_NO_KEY,
};
use crate::command::zset_util::eq_ignore_case;
use crate::command::Ctx;
use crate::ds::{expire, vectorset_ds};
use crate::resp::codec::{append_array, append_bulk, append_error, append_null};

/// Cosine similarity of two equal-length vectors; a zero vector has no
/// direction, so any pair touching one scores 0 (-> VSIM score 0.5).
fn cosine(a: &[f64], b: &[f64]) -> f64 {
    let (mut dot, mut na, mut nb) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// VSIM key [COUNT n] [WITHSCORES] [WITHATTRIBS] (FP16 <blob> |
/// VALUES <v...>): options parse in ANY order, the vector spec last
/// (VALUES swallows everything after it). Missing key -> error (Redis
/// answers nil; documented deviation). Reply is a flat array: elements,
/// with attr and score interleaved per element when the flags ask for
/// them (both -> element, attr, score).
pub async fn vsim(ctx: &mut Ctx<'_>) {
    if ctx.args.is_empty() {
        arity(ctx.out, "vsim");
        return;
    }
    let key = ctx.args[0].clone();
    let dim = match vectorset_state(&ctx.shared.store, &ctx.prefix_key, &key, expire::now_ms()) {
        VectorSetState::WrongType => {
            append_error(ctx.out, WRONGTYPE);
            return;
        }
        VectorSetState::Missing => {
            append_error(ctx.out, ERR_NO_KEY);
            return;
        }
        VectorSetState::VectorSet { dim, .. } => dim,
    };
    let mut limit: Option<u64> = None;
    let (mut want_scores, mut want_attribs) = (false, false);
    let mut query: Option<Vec<f64>> = None;
    let mut i = 1;
    while i < ctx.args.len() {
        let arg = ctx.args[i].as_slice();
        if eq_ignore_case(arg, b"COUNT") {
            let Some(raw) = ctx.args.get(i + 1) else {
                arity(ctx.out, "vsim");
                return;
            };
            match std::str::from_utf8(raw)
                .ok()
                .and_then(|t| t.parse::<u64>().ok())
            {
                Some(n) => limit = Some(n),
                None => {
                    append_error(ctx.out, ERR_COUNT);
                    return;
                }
            }
            i += 2;
        } else if eq_ignore_case(arg, b"WITHSCORES") {
            want_scores = true;
            i += 1;
        } else if eq_ignore_case(arg, b"WITHATTRIBS") {
            want_attribs = true;
            i += 1;
        } else if eq_ignore_case(arg, b"FP16") {
            let Some(blob) = ctx.args.get(i + 1) else {
                arity(ctx.out, "vsim");
                return;
            };
            match parse_vector(b"FP16", std::slice::from_ref(blob), dim) {
                Ok(v) => query = Some(v),
                Err(e) => {
                    append_error(ctx.out, e);
                    return;
                }
            }
            i += 2;
        } else if eq_ignore_case(arg, b"VALUES") {
            // Consumes ALL remaining args as the vector.
            match parse_vector(b"VALUES", &ctx.args[i + 1..], dim) {
                Ok(v) => query = Some(v),
                Err(e) => {
                    append_error(ctx.out, e);
                    return;
                }
            }
            i = ctx.args.len();
        } else {
            // Unknown option: EF/EXPLORE/FILTER are unimplemented and
            // land here too (documented deviation).
            arity(ctx.out, "vsim");
            return;
        }
    }
    let Some(query) = query else {
        arity(ctx.out, "vsim");
        return;
    };
    let mut hits: Vec<(Vec<u8>, f64, Option<Vec<u8>>)> = Vec::new();
    let scanned = vectorset_ds::for_each_elem(
        &ctx.shared.store,
        &ctx.prefix_key,
        &key,
        &mut |element, value| {
            if let Some((vector, attr)) = vectorset_ds::decode_elem_value(value, dim) {
                hits.push((
                    element.to_vec(),
                    (cosine(&query, &vector) + 1.0) / 2.0,
                    attr,
                ));
            }
            true
        },
    );
    if scanned.is_err() {
        append_error(ctx.out, "ERR: vsim failed");
        return;
    }
    // score DESC, element bytes ASC on ties; total_cmp keeps NaN-order
    // deterministic should a stored vector hold non-finite values.
    hits.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if let Some(n) = limit {
        hits.truncate(n as usize);
    }
    let per = 1 + usize::from(want_scores) + usize::from(want_attribs);
    append_array(ctx.out, hits.len() * per);
    for (element, score, attr) in &hits {
        append_bulk(ctx.out, element);
        if want_attribs {
            match attr {
                Some(a) => append_bulk(ctx.out, a),
                None => append_null(ctx.out),
            }
        }
        if want_scores {
            // Shortest-roundtrip f64: 1.0 -> "1", 0.5 -> "0.5".
            append_bulk(ctx.out, format!("{score}").as_bytes());
        }
    }
}
