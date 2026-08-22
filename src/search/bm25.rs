//! Okapi BM25 scoring and the bounded top-k collector, as pure
//! functions. Node-local semantics: `n` (corpus size), `df` and
//! `avgdl` are the values of the single slot this index lives in --
//! cross-node fan-out merges top-k lists, it never merges statistics
//! (documented in COMPAT.md).
//!
//! idf(t) = ln(1 + (n - df + 0.5) / (df + 0.5))  -- Lucene/RediSearch
//! flavor, always positive (a term in EVERY document still scores > 0,
//! unlike raw Okapi which can go negative).

pub const K1: f64 = 1.2;
pub const B: f64 = 0.75;

/// Inverse document frequency of a term seen in `df` of `n` docs.
pub fn idf(df: u64, n: u64) -> f64 {
    let (df, n) = (df.max(1) as f64, n.max(1) as f64);
    (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
}

/// One term's contribution for a document.
///
/// `tf` = term occurrences in the doc, `doclen` = the doc's total term
/// count (all fields), `avgdl` = corpus mean doclen.
pub fn term_score(tf: u64, df: u64, doclen: u64, avgdl: f64, n: u64) -> f64 {
    let tf = tf as f64;
    let norm = 1.0 - B + B * (doclen.max(1) as f64) / avgdl.max(1e-9);
    idf(df, n) * (tf * (K1 + 1.0)) / (tf + K1 * norm)
}

/// A scored hit; ties break by ascending docid bytes (deterministic
/// across replica nodes).
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub docid: Vec<u8>,
    pub score: f64,
}

/// Bounded top-k collector over `(docid, score)` pushes. Keeps the k
/// best by score; ties prefer the SMALLER docid, mirroring how a sort
/// by `(-score, docid)` behaves. `k` of 0 collects nothing.
#[derive(Debug, Default)]
pub struct TopK {
    hits: Vec<Hit>,
    capacity: usize,
}

impl TopK {
    pub fn new(k: usize) -> TopK {
        TopK {
            hits: Vec::with_capacity(k.min(64)),
            capacity: k,
        }
    }

    pub fn push(&mut self, docid: &[u8], score: f64) {
        if self.capacity == 0 || !score.is_finite() {
            return;
        }
        let full = self.hits.len() >= self.capacity;
        if full {
            let worst = self.worst_idx();
            let beats_worst = score > self.hits[worst].score
                || (score == self.hits[worst].score && docid < self.hits[worst].docid.as_slice());
            if !beats_worst {
                return;
            }
            self.hits[worst] = Hit {
                docid: docid.to_vec(),
                score,
            };
            return;
        }
        self.hits.push(Hit {
            docid: docid.to_vec(),
            score,
        });
    }

    /// Index of the current worst hit (lowest score, largest docid on
    /// ties) -- the eviction candidate.
    fn worst_idx(&self) -> usize {
        let mut worst = 0;
        for (i, h) in self.hits.iter().enumerate().skip(1) {
            if h.score < self.hits[worst].score
                || (h.score == self.hits[worst].score && h.docid > self.hits[worst].docid)
            {
                worst = i;
            }
        }
        worst
    }

    /// Final ranking: score desc, docid asc; consumes the collector.
    pub fn finish(mut self) -> Vec<Hit> {
        self.hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap()
                .then_with(|| a.docid.cmp(&b.docid))
        });
        self.hits
    }

    pub fn len(&self) -> usize {
        self.hits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idf_is_positive_and_decreasing_in_df() {
        assert!(idf(1, 100) > idf(50, 100));
        assert!(idf(100, 100) > 0.0); // Lucene flavor stays positive
        assert!(idf(0, 0) > 0.0); // degenerate corpus does not NaN
    }

    /// Hand-computed parity check: n=4 docs, df=2, tf=3, doclen=10,
    /// avgdl=10 -> norm = 1 (doclen == avgdl).
    #[test]
    fn term_score_matches_hand_computation() {
        let expected = idf(2, 4) * (3.0 * (K1 + 1.0)) / (3.0 + K1 * 1.0);
        assert!((term_score(3, 2, 10, 10.0, 4) - expected).abs() < 1e-12);
    }

    #[test]
    fn longer_docs_are_penalized() {
        assert!(term_score(3, 2, 10, 10.0, 4) > term_score(3, 2, 500, 10.0, 4));
        // tf saturation: doubling tf less than doubles the score.
        let s1 = term_score(2, 2, 10, 10.0, 4);
        let s2 = term_score(4, 2, 10, 10.0, 4);
        assert!(s2 < 2.0 * s1);
    }

    #[test]
    fn topk_orders_and_evicts() {
        let mut top = TopK::new(2);
        top.push(b"a", 1.0);
        top.push(b"b", 3.0);
        top.push(b"c", 2.0);
        top.push(b"d", 2.0); // ties with c but d > c (docid asc): rejected
        top.push(b"e", 3.0); // beats c outright: c evicted
        let hits = top.finish();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].docid, b"b".to_vec()); // tie 3.0 -> docid asc
        assert_eq!(hits[1].docid, b"e".to_vec());
        assert!((hits[0].score - 3.0).abs() < 1e-12);
    }

    #[test]
    fn topk_zero_capacity_is_inert() {
        let mut top = TopK::new(0);
        top.push(b"a", 9.0);
        assert!(top.is_empty());
        assert!(top.finish().is_empty());
    }
}
