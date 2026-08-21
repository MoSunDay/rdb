//! ANN physical records: the single centroid table record (with SQ8
//! calibration) and the per-partition quantized posting entries, plus
//! the incremental membership ops FT.ADD/FT.DEL drive between builds.

use rocksdb::WriteBatch;

use crate::ds::codec::{decode_data_key, encode_count};
use crate::store::{ops, Store};

use crate::search::index_codec::{ann_posting_key, centroid_key, decode_centroids, CentroidTable};
use crate::search::quant::{self, Calibration};

use super::kmeans::nearest;
use super::SqEntry;

/// Leading LEB128 count and the remainder (`codec::decode_count`
/// semantics but returning the rest slice).
fn take_count(payload: &[u8]) -> Option<(u64, &[u8])> {
    let mut v = 0u64;
    for (i, &b) in payload.iter().enumerate() {
        if i < 9 {
            v |= u64::from(b & 0x7f) << (7 * i);
        }
        if b & 0x80 == 0 {
            return Some((v, &payload[i + 1..]));
        }
    }
    None
}

pub(super) fn read_table(
    store: &Store,
    prefix: &[u8],
    index: &[u8],
    field: &[u8],
) -> Result<Option<CentroidTable>, String> {
    match ops::get_physical(store, &centroid_key(prefix, index, field))? {
        None => Ok(None),
        Some(v) => decode_centroids(&v)
            .map(Some)
            .ok_or_else(|| "ERR corrupt centroid record".to_string()),
    }
}

/// Partition value = LEB128(dim) ++ LEB128(n) ++ entries, entries
/// docid-sorted: LEB128(docid_len) ++ docid ++ dim SQ8 bytes.
pub(super) fn encode_partition(dim: u64, entries: &[SqEntry]) -> Vec<u8> {
    let mut out = encode_count(dim);
    out.extend_from_slice(&encode_count(entries.len() as u64));
    for e in entries {
        out.extend_from_slice(&encode_count(e.docid.len() as u64));
        out.extend_from_slice(&e.docid);
        out.extend_from_slice(&e.q);
    }
    out
}

fn decode_partition(value: &[u8]) -> Option<(u64, Vec<SqEntry>)> {
    let (dim, rest) = take_count(value)?;
    let (n, mut rest) = take_count(rest)?;
    let mut out = Vec::with_capacity(n.min(1 << 20) as usize);
    for _ in 0..n {
        let (dlen, r) = take_count(rest)?;
        let docid = r.get(..dlen as usize)?.to_vec();
        let r = r.get(dlen as usize..)?;
        let q = r.get(..dim as usize)?.to_vec();
        rest = r.get(dim as usize..)?;
        out.push(SqEntry { docid, q });
    }
    Some((dim, out))
}

pub(super) fn read_partition(
    store: &Store,
    prefix: &[u8],
    index: &[u8],
    field: &[u8],
    cid: u64,
) -> Result<Vec<SqEntry>, String> {
    match ops::get_physical(store, &ann_posting_key(prefix, index, field, cid))? {
        None => Ok(Vec::new()),
        Some(v) => decode_partition(&v)
            .map(|(_, entries)| entries)
            .ok_or_else(|| "ERR corrupt ANN partition".to_string()),
    }
}

/// The docid of a doc-record physical key (`LEB128(len) ++ docid`
/// suffix after the codec's data-key header). Shared with the
/// match-all scan path in `ft_search`.
pub fn docid_of_key(physical: &[u8], prefix_len: usize) -> Option<Vec<u8>> {
    let suffix = decode_data_key(physical, prefix_len)?.2;
    let (dlen, rest) = take_count(suffix)?;
    Some(rest.get(..dlen as usize)?.to_vec())
}

/// Nearest-centroid id for `vector`; `None` until FT.BUILD ran.
pub fn assign(
    store: &Store,
    prefix: &[u8],
    index: &[u8],
    field: &[u8],
    _dim: u64,
    vector: &[f64],
) -> Result<Option<u64>, String> {
    let Some(table) = read_table(store, prefix, index, field)? else {
        return Ok(None);
    };
    let centroids: Vec<Vec<f64>> = table
        .centroids
        .iter()
        .map(|c| c.iter().map(|&x| f64::from(x)).collect())
        .collect();
    let (i, _) = nearest(&centroids, vector);
    Ok(Some(i as u64))
}

/// The SQ8 calibration stored in the centroid record, as a value.
pub(super) fn calibration_of(table: &CentroidTable) -> Calibration {
    Calibration {
        min: table.min.clone(),
        scale: table.scale.clone(),
    }
}

#[allow(clippy::too_many_arguments)] // explicit slices beat a param struct here
/// Upsert (docid, SQ8(vector)) into partition `cid`.
pub fn partition_add(
    batch: &mut WriteBatch,
    store: &Store,
    prefix: &[u8],
    index: &[u8],
    field: &[u8],
    dim: u64,
    cid: u64,
    docid: &[u8],
    vector: &[f64],
) -> Result<(), String> {
    let table =
        read_table(store, prefix, index, field)?.ok_or("ERR no ANN index (run FT.BUILD)")?;
    let mut entries = read_partition(store, prefix, index, field, cid)?;
    let q = quant::encode(&calibration_of(&table), vector);
    match entries.binary_search_by(|e| e.docid.as_slice().cmp(docid)) {
        Ok(i) => entries[i].q = q,
        Err(i) => entries.insert(
            i,
            SqEntry {
                docid: docid.to_vec(),
                q,
            },
        ),
    }
    batch.put(
        ann_posting_key(prefix, index, field, cid),
        encode_partition(dim, &entries),
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)] // explicit slices beat a param struct here
/// Remove `docid` from partition `cid` (delete the record when empty).
pub fn partition_remove(
    batch: &mut WriteBatch,
    store: &Store,
    prefix: &[u8],
    index: &[u8],
    field: &[u8],
    dim: u64,
    cid: u64,
    docid: &[u8],
) -> Result<(), String> {
    let mut entries = read_partition(store, prefix, index, field, cid)?;
    match entries.binary_search_by(|e| e.docid.as_slice().cmp(docid)) {
        Ok(i) => {
            entries.remove(i);
        }
        Err(_) => return Ok(()), // never joined this partition
    }
    let key = ann_posting_key(prefix, index, field, cid);
    if entries.is_empty() {
        batch.delete(key);
    } else {
        batch.put(key, encode_partition(dim, &entries));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_codec_roundtrip() {
        let entries = vec![
            SqEntry {
                docid: b"a".to_vec(),
                q: vec![1, 2],
            },
            SqEntry {
                docid: b"b".to_vec(),
                q: vec![3, 4],
            },
        ];
        let raw = encode_partition(2, &entries);
        assert_eq!(decode_partition(&raw).unwrap(), (2, entries));
    }
}
