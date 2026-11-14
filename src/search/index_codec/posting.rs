//! Posting-list, termstat and centroid-table value codecs (the
//! `count ++ sorted entries` half of the family). Key builders and
//! the meta/doc records live in the parent; LEB128 helpers
//! (`encode_count`, `take_count`) are shared through it.

use crate::ds::codec::encode_count;

use super::take_count;

#[derive(Debug, Clone, PartialEq)]
pub struct TermStat {
    pub df: u64,
    pub total_tf: u64,
}

/// SQ8 calibration + centroids; members[i] counts docs in partition i.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CentroidTable {
    pub dim: u64,
    pub centroids: Vec<Vec<f32>>,
    pub min: Vec<f32>,
    pub scale: Vec<f32>,
    pub members: Vec<u64>,
}

/// Doc-entry of a posting value, docid-sorted by writer. `positions`
/// are the doc's token ordinals (0-based, per field, strictly
/// increasing); EMPTY means unknown -- v1 records carry none.
#[derive(Debug, Clone, PartialEq)]
pub struct PostEntry {
    pub docid: Vec<u8>,
    pub tf: u64,
    pub positions: Vec<u64>,
}

/// v1 value (kind 0x15): `count ++ entries[ docid_len ++ docid ++ tf ]`.
pub fn encode_posting(entries: &[PostEntry]) -> Vec<u8> {
    let mut out = encode_count(entries.len() as u64);
    for e in entries {
        out.extend_from_slice(&encode_count(e.docid.len() as u64));
        out.extend_from_slice(&e.docid);
        out.extend_from_slice(&encode_count(e.tf));
    }
    out
}

pub fn decode_posting(value: &[u8]) -> Option<Vec<PostEntry>> {
    let (n, mut rest) = take_count(value)?;
    let mut out = Vec::with_capacity(n.min(1 << 20) as usize);
    for _ in 0..n {
        let (dlen, r) = take_count(rest)?;
        let docid = r.get(..dlen as usize)?.to_vec();
        let r = r.get(dlen as usize..)?;
        let (tf, r) = take_count(r)?;
        rest = r;
        out.push(PostEntry {
            docid,
            tf,
            positions: Vec::new(), // v1 never stored positions
        });
    }
    Some(out)
}

/// v2 value (kind 0x1A):
/// `count ++ entries[ docid_len ++ docid ++ tf ++ npos ++ Δpositions ]`.
///
/// Positions are delta-encoded as LEB128 of `pos[i] - pos[i-1] - 1`
/// (first delta is `pos[0]` itself), i.e. STRICTLY-INCREASING inputs
/// encode losslessly; `npos = 0` means unknown (v1 semantics).
/// INVARIANT (not enforced here): writers pass sorted-by-construction
/// ordinals -- `tokenize` enumeration order. Non-increasing input
/// would underflow-saturate the delta and decode differently; the
/// writers own this invariant, decode returns what was stored.
pub fn encode_posting2(entries: &[PostEntry]) -> Vec<u8> {
    let mut out = encode_count(entries.len() as u64);
    for e in entries {
        out.extend_from_slice(&encode_count(e.docid.len() as u64));
        out.extend_from_slice(&e.docid);
        out.extend_from_slice(&encode_count(e.tf));
        out.extend_from_slice(&encode_count(e.positions.len() as u64));
        let mut prev = 0u64;
        let mut first = true;
        for &pos in &e.positions {
            if first {
                out.extend_from_slice(&encode_count(pos));
                first = false;
            } else {
                out.extend_from_slice(&encode_count(pos.wrapping_sub(prev).wrapping_sub(1)));
            }
            prev = pos;
        }
    }
    out
}

pub fn decode_posting2(value: &[u8]) -> Option<Vec<PostEntry>> {
    let (n, mut rest) = take_count(value)?;
    let mut out = Vec::with_capacity(n.min(1 << 20) as usize);
    for _ in 0..n {
        let (dlen, r) = take_count(rest)?;
        let docid = r.get(..dlen as usize)?.to_vec();
        let r = r.get(dlen as usize..)?;
        let (tf, r) = take_count(r)?;
        let (npos, mut r) = take_count(r)?;
        let mut positions = Vec::with_capacity(npos.min(1 << 20) as usize);
        let mut prev = 0u64;
        for i in 0..npos {
            let (d, r2) = take_count(r)?;
            let pos = if i == 0 {
                d
            } else {
                prev.wrapping_add(d).wrapping_add(1)
            };
            positions.push(pos);
            prev = pos;
            r = r2;
        }
        rest = r;
        out.push(PostEntry {
            docid,
            tf,
            positions,
        });
    }
    Some(out)
}

/// Upsert one (docid, tf, positions) keeping bytewise docid order;
/// `positions` REPLACE the entry's list on both insert and update (a
/// re-added doc's positions are the new ones, never merged). Returns
/// whether the entry is new (df delta).
pub fn upsert_posting(
    entries: &mut Vec<PostEntry>,
    docid: &[u8],
    tf: u64,
    positions: &[u64],
) -> bool {
    match entries.binary_search_by(|e| e.docid.as_slice().cmp(docid)) {
        Ok(i) => {
            entries[i].tf = tf;
            entries[i].positions = positions.to_vec();
            false
        }
        Err(i) => {
            entries.insert(
                i,
                PostEntry {
                    docid: docid.to_vec(),
                    tf,
                    positions: positions.to_vec(),
                },
            );
            true
        }
    }
}

/// Remove one docid; returns whether it was present.
pub fn remove_posting(entries: &mut Vec<PostEntry>, docid: &[u8]) -> bool {
    match entries.binary_search_by(|e| e.docid.as_slice().cmp(docid)) {
        Ok(i) => {
            entries.remove(i);
            true
        }
        Err(_) => false,
    }
}

pub fn encode_termstat(stat: &TermStat) -> Vec<u8> {
    [encode_count(stat.df), encode_count(stat.total_tf)].concat()
}

pub fn decode_termstat(value: &[u8]) -> Option<TermStat> {
    let (df, rest) = take_count(value)?;
    let (total_tf, _) = take_count(rest)?;
    Some(TermStat { df, total_tf })
}

pub fn encode_centroids(table: &CentroidTable) -> Vec<u8> {
    let mut out = encode_count(table.centroids.len() as u64);
    out.extend_from_slice(&encode_count(table.dim));
    for c in &table.centroids {
        for &x in c.iter().take(table.dim as usize) {
            out.extend_from_slice(&x.to_le_bytes());
        }
    }
    for &x in table.min.iter().take(table.dim as usize) {
        out.extend_from_slice(&x.to_le_bytes());
    }
    for &x in table.scale.iter().take(table.dim as usize) {
        out.extend_from_slice(&x.to_le_bytes());
    }
    for &m in &table.members {
        out.extend_from_slice(&encode_count(m));
    }
    out
}

pub fn decode_centroids(value: &[u8]) -> Option<CentroidTable> {
    let (k, rest) = take_count(value)?;
    let (dim, mut rest) = take_count(rest)?;
    let dimusize = dim as usize;
    if rest.len() < k as usize * dimusize * 4 {
        return None;
    }
    let mut centroids = Vec::with_capacity(k.min(65536) as usize);
    for _ in 0..k {
        let mut c = Vec::with_capacity(dimusize);
        for chunk in rest[..dimusize * 4].as_chunks::<4>().0 {
            c.push(f32::from_le_bytes(*chunk));
        }
        rest = &rest[dimusize * 4..];
        centroids.push(c);
    }
    fn take_axes(dimusize: usize, rest: &[u8]) -> Option<(Vec<f32>, &[u8])> {
        if rest.len() < dimusize * 4 {
            return None;
        }
        let axes = rest[..dimusize * 4]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect();
        Some((axes, &rest[dimusize * 4..]))
    }
    let (min, rest) = take_axes(dimusize, rest)?;
    let (scale, mut rest) = take_axes(dimusize, rest)?;
    let mut members = Vec::with_capacity(k.min(65536) as usize);
    for _ in 0..k {
        let (m, r) = take_count(rest)?;
        rest = r;
        members.push(m);
    }
    Some(CentroidTable {
        dim,
        centroids,
        min,
        scale,
        members,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posting_upsert_keeps_docid_order() {
        let mut entries = vec![];
        assert!(upsert_posting(&mut entries, b"c", 2, &[3]));
        assert!(upsert_posting(&mut entries, b"a", 1, &[0, 2]));
        assert!(!upsert_posting(&mut entries, b"a", 5, &[0, 4])); // replace, not merge
        assert_eq!(
            entries,
            vec![
                PostEntry {
                    docid: b"a".to_vec(),
                    tf: 5,
                    positions: vec![0, 4],
                },
                PostEntry {
                    docid: b"c".to_vec(),
                    tf: 2,
                    positions: vec![3],
                },
            ]
        );
        let raw = encode_posting(&entries);
        // v1 wire keeps (docid, tf) and DROPS positions on decode.
        assert_eq!(
            decode_posting(&raw).unwrap(),
            vec![
                PostEntry {
                    docid: b"a".to_vec(),
                    tf: 5,
                    positions: vec![],
                },
                PostEntry {
                    docid: b"c".to_vec(),
                    tf: 2,
                    positions: vec![],
                },
            ]
        );
        assert!(remove_posting(&mut entries, b"c"));
        assert!(!remove_posting(&mut entries, b"c"));
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn posting2_roundtrip_empty_and_multi_positions() {
        let entries = vec![
            PostEntry {
                docid: b"d1".to_vec(),
                tf: 3,
                positions: vec![0, 3, 7], // deltas 0, 2, 3
            },
            PostEntry {
                docid: b"d2".to_vec(),
                tf: 1,
                positions: vec![], // unknown/v1 semantics: npos = 0
            },
        ];
        let raw = encode_posting2(&entries);
        assert_eq!(decode_posting2(&raw).unwrap(), entries);
    }

    #[test]
    fn posting2_delta_roundtrip_edges() {
        // single far-out position: first delta is pos[0] itself
        let entries = vec![PostEntry {
            docid: b"d".to_vec(),
            tf: 1,
            positions: vec![5],
        }];
        assert_eq!(
            decode_posting2(&encode_posting2(&entries)).unwrap(),
            entries
        );
        // adjacent ordinals: deltas of 0 (`pos[i] - pos[i-1] - 1`)
        let entries = vec![PostEntry {
            docid: b"d".to_vec(),
            tf: 2,
            positions: vec![0, 1],
        }];
        assert_eq!(
            decode_posting2(&encode_posting2(&entries)).unwrap(),
            entries
        );
    }

    #[test]
    fn posting2_truncated_inputs_reject() {
        let entries = vec![PostEntry {
            docid: b"d1".to_vec(),
            tf: 2,
            positions: vec![0, 9],
        }];
        let raw = encode_posting2(&entries);
        for cut in 0..raw.len() {
            assert_eq!(decode_posting2(&raw[..cut]), None, "cut at {cut}");
        }
        // v1 codec rejects v2 truncation the same way
        let v1 = encode_posting(&entries);
        for cut in 0..v1.len() {
            assert_eq!(decode_posting(&v1[..cut]), None, "cut at {cut}");
        }
    }

    #[test]
    fn centroids_roundtrip() {
        let table = CentroidTable {
            dim: 2,
            centroids: vec![vec![0.5, -0.5], vec![1.0, 2.0]],
            min: vec![-1.0, 0.0],
            scale: vec![0.01, 0.02],
            members: vec![3, 4],
        };
        assert_eq!(decode_centroids(&encode_centroids(&table)).unwrap(), table);
    }
}
