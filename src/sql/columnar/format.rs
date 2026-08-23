//! Segment file envelope: `MAGIC | page bytes | footer JSON | footer_len(u32 BE) | crc32(u32 BE)`.
//! The crc32 covers the footer JSON bytes; the footer carries the full
//! page index (per column: file offset/len, value count, first ordinal,
//! encoding, zonemap). Footer JSON via serde_json (already a dep); the
//! CRC-32/IEEE is hand-rolled (zero new dependencies).

use serde::{Deserialize, Serialize};

use crate::sql::storage::schema::Value;

/// File magic of every segment file (first 8 bytes).
pub const MAGIC: &[u8; 8] = b"RDBCOL01";
/// Footer format version stamped into [`Footer::version`].
pub const FOOTER_VERSION: u32 = 1;
/// Trailer size after the footer JSON: footer_len(u32 BE) + crc32(u32 BE).
const TRAILER_LEN: usize = 8;

/// Hand-rolled CRC-32/IEEE (reflected, poly 0xEDB88320).
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Index + zonemap of one page inside the footer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageFooter {
    /// Page bytes start (absolute file offset).
    pub offset: u64,
    /// Page byte length.
    pub len: u32,
    pub num_values: u32,
    /// Ordinal of the page's first value within the column.
    pub first_ordinal: u32,
    /// `encode::ENC_PLAIN` / `encode::ENC_DICT`.
    pub encoding: String,
    pub null_count: u32,
    /// Zonemap over the page (Null when the page is all-NULL).
    pub min: Value,
    pub max: Value,
}

/// All pages of one column.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnFooter {
    pub name: String,
    pub pages: Vec<PageFooter>,
}

/// Segment footer: version, row count and the full page index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Footer {
    pub version: u32,
    pub num_rows: u64,
    pub columns: Vec<ColumnFooter>,
}

/// Assemble the final file bytes: MAGIC | body | footer JSON |
/// footer_len | crc32(footer JSON).
pub fn assemble_file(footer: &Footer, body: &[u8]) -> Result<Vec<u8>, String> {
    let footer_json = serde_json::to_vec(footer).map_err(|e| format!("footer serialize: {e}"))?;
    let footer_len = u32::try_from(footer_json.len())
        .map_err(|_| format!("footer too large ({} bytes)", footer_json.len()))?;
    let mut out = Vec::with_capacity(MAGIC.len() + body.len() + footer_json.len() + TRAILER_LEN);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(body);
    out.extend_from_slice(&footer_json);
    out.extend_from_slice(&footer_len.to_be_bytes());
    out.extend_from_slice(&crc32(&footer_json).to_be_bytes());
    Ok(out)
}

/// Split + validate one file: magic, trailer layout, footer_len sanity,
/// crc, JSON parse. Returns the page region (between MAGIC and the
/// footer) plus the footer. Errors name the exact corruption class.
pub fn split_file(bytes: &[u8]) -> Result<(&[u8], Footer), String> {
    if bytes.len() < MAGIC.len() {
        return Err("truncated segment: shorter than magic".to_string());
    }
    if &bytes[..MAGIC.len()] != MAGIC {
        return Err("bad magic".to_string());
    }
    if bytes.len() < MAGIC.len() + TRAILER_LEN {
        return Err("truncated segment: missing trailer".to_string());
    }
    let trailer = &bytes[bytes.len() - TRAILER_LEN..];
    let footer_len = u32::from_be_bytes(trailer[..4].try_into().unwrap()) as usize;
    let want_crc = u32::from_be_bytes(trailer[4..].try_into().unwrap());
    let max_footer = bytes.len() - MAGIC.len() - TRAILER_LEN;
    if footer_len > max_footer {
        return Err(format!(
            "footer length out of range ({footer_len} > {max_footer})"
        ));
    }
    let footer_start = bytes.len() - TRAILER_LEN - footer_len;
    let footer_json = &bytes[footer_start..bytes.len() - TRAILER_LEN];
    if crc32(footer_json) != want_crc {
        return Err("footer crc mismatch".to_string());
    }
    let footer: Footer =
        serde_json::from_slice(footer_json).map_err(|e| format!("footer json: {e}"))?;
    Ok((&bytes[MAGIC.len()..footer_start], footer))
}
