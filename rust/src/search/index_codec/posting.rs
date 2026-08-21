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

/// Doc-entry of a posting value: (docid, tf), docid-sorted by writer.
#[derive(Debug, Clone, PartialEq)]
pub struct PostEntry {
    pub docid: Vec<u8>,
    pub tf: u64,
}

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
        out.push(PostEntry { docid, tf });
    }
    Some(out)
}

/// Upsert one (docid, tf) keeping bytewise docid order; returns whether
/// the entry is new (df delta).
pub fn upsert_posting(entries: &mut Vec<PostEntry>, docid: &[u8], tf: u64) -> bool {
    match entries.binary_search_by(|e| e.docid.as_slice().cmp(docid)) {
        Ok(i) => {
            entries[i].tf = tf;
            false
        }
        Err(i) => {
            entries.insert(
                i,
                PostEntry {
                    docid: docid.to_vec(),
                    tf,
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
    for axis in 0..table.dim as usize {
        out.extend_from_slice(&table.min[axis].to_le_bytes());
    }
    for axis in 0..table.dim as usize {
        out.extend_from_slice(&table.scale[axis].to_le_bytes());
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
        for chunk in rest[..dimusize * 4].chunks_exact(4) {
            c.push(f32::from_le_bytes(chunk.try_into().ok()?));
        }
        rest = &rest[dimusize * 4..];
        centroids.push(c);
    }
    fn take_axes(dimusize: usize, rest: &[u8]) -> Option<(Vec<f32>, &[u8])> {
        if rest.len() < dimusize * 4 {
            return None;
        }
        let axes = rest[..dimusize * 4]
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
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
        assert!(upsert_posting(&mut entries, b"c", 2));
        assert!(upsert_posting(&mut entries, b"a", 1));
        assert!(!upsert_posting(&mut entries, b"a", 5));
        assert_eq!(
            entries,
            vec![
                PostEntry {
                    docid: b"a".to_vec(),
                    tf: 5
                },
                PostEntry {
                    docid: b"c".to_vec(),
                    tf: 2
                }
            ]
        );
        let raw = encode_posting(&entries);
        assert_eq!(decode_posting(&raw).unwrap(), entries);
        assert!(remove_posting(&mut entries, b"c"));
        assert!(!remove_posting(&mut entries, b"c"));
        assert_eq!(entries.len(), 1);
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
