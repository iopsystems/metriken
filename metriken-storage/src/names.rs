//! A sealed segment's names fingerprint, kept in dendro's per-segment
//! `caller_index` so a reader can learn a table's metric names from one
//! segment footer per distinct fingerprint rather than every footer.
//!
//! The fingerprint covers each column's data type and metric name (its
//! `metric` metadata, else its column name). Two segments with the same
//! fingerprint hold the same metric names, so a reader needs to parse the
//! footer of only one of them. It is not a digest of the columns: two
//! segments of a wide table whose per-slot columns differ but whose metrics do
//! not share a fingerprint, which is the point.

use std::collections::BTreeSet;

/// Prefix naming the index as a names fingerprint, version 1, so a
/// `caller_index` written by another producer is not mistaken for one.
const MAGIC: &[u8; 4] = b"MNS1";

/// The index to store with a sealed segment, or `None` when its footer
/// cannot be read (the segment is then read as unindexed).
pub(crate) fn index_of(segment: &[u8]) -> Option<Vec<u8>> {
    let bytes = bytes::Bytes::copy_from_slice(segment);
    let meta = parquet::arrow::arrow_reader::ArrowReaderMetadata::load(
        &bytes,
        parquet::arrow::arrow_reader::ArrowReaderOptions::default(),
    )
    .ok()?;
    let names: BTreeSet<String> = meta
        .schema()
        .fields()
        .iter()
        .map(|f| {
            let name = f.metadata().get("metric").unwrap_or(f.name());
            format!("{:?}\u{0}{name}", f.data_type())
        })
        .collect();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for name in &names {
        for b in name.bytes().chain([0xff]) {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    let mut index = MAGIC.to_vec();
    index.extend_from_slice(&hash.to_le_bytes());
    Some(index)
}

/// The fingerprint a stored index holds, or `None` for an index that is not
/// a names fingerprint (none at all, or another producer's).
pub(crate) fn fingerprint(index: Option<&[u8]>) -> Option<u64> {
    let rest = index?.strip_prefix(MAGIC.as_slice())?;
    Some(u64::from_le_bytes(rest.try_into().ok()?))
}
