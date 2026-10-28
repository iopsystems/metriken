//! A sealed segment's names index, kept as dendro's per-segment caller index
//! (`SegmentRow::caller_index`: a row of `segment_indexes` in a schema 5
//! archive, the `segments.caller_index` column in a schema 4 one), so a
//! reader learns a table's metric names, and its first segment's interval,
//! from the catalog without fetching a segment.
//!
//! The writer stores a [`SegmentNames`]: the metric names the segment's
//! footer holds by kind, its `sampling_interval_ms` and its `metriken.format`,
//! which is what opening the segment as a probe reads. Segments sealed
//! before it carry a names fingerprint instead (or nothing), which a reader
//! still uses: two segments with the same fingerprint hold the same metric
//! names, so the reader parses the footer of one of them.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{BufferPool, DataSource};

/// Prefix naming the index as a names fingerprint, version 1, so a
/// `caller_index` written by another producer is not mistaken for one.
const FINGERPRINT: &[u8; 4] = b"MNS1";

/// Prefix naming the index as a [`SegmentNames`], version 1: msgpack,
/// compressed with zstd.
const NAMES: &[u8; 4] = b"MNI1";

/// What a segment's footer gives a reader at open: the metric names it
/// holds by kind, and the two footer keys the open reads.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SegmentNames {
    pub counters: BTreeSet<String>,
    pub gauges: BTreeSet<String>,
    pub histograms: BTreeSet<String>,
    pub gauge_histograms: BTreeSet<String>,
    /// The footer's `sampling_interval_ms`, as written.
    pub sampling_interval_ms: Option<String>,
    /// The footer's `metriken.format`, as written.
    pub format: Option<String>,
}

/// A stored index, as a reader uses it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Stored {
    Names(SegmentNames),
    /// A names fingerprint: the segment holds the same names as any other
    /// with this value.
    Fingerprint(u64),
}

/// The index to store with a sealed segment, or `None` when its footer
/// cannot be read (the segment is then read as unindexed).
pub(crate) fn index_of(segment: &[u8]) -> Option<Vec<u8>> {
    encode(&names_of(segment)?)
}

/// The names a probe of `segment` reads, as the reader's open does.
pub(crate) fn names_of(segment: &[u8]) -> Option<SegmentNames> {
    // A footer-only read: no page is decoded, so the pool stays empty.
    let probe = crate::parquet::MultiParquetSource::open_bytes_with_pool(
        segment.to_vec(),
        BufferPool::new(1024 * 1024),
    )
    .ok()?;
    Some(SegmentNames {
        counters: probe.counter_names().into_iter().collect(),
        gauges: probe.gauge_names().into_iter().collect(),
        histograms: probe.histogram_names().into_iter().collect(),
        gauge_histograms: probe.gauge_histogram_names().into_iter().collect(),
        sampling_interval_ms: probe.metadata_get("sampling_interval_ms"),
        format: probe.metadata_get(crate::format::FORMAT_KEY),
    })
}

fn encode(names: &SegmentNames) -> Option<Vec<u8>> {
    let packed = rmp_serde::to_vec(names).ok()?;
    let mut index = NAMES.to_vec();
    index.extend(zstd::stream::encode_all(packed.as_slice(), 3).ok()?);
    Some(index)
}

/// What a stored index holds, or `None` for an index that is neither (none
/// at all, another producer's, or one that does not decode).
pub(crate) fn read(index: Option<&[u8]>) -> Option<Stored> {
    let index = index?;
    if let Some(rest) = index.strip_prefix(NAMES.as_slice()) {
        let packed = zstd::stream::decode_all(rest).ok()?;
        return rmp_serde::from_slice(&packed).ok().map(Stored::Names);
    }
    let rest = index.strip_prefix(FINGERPRINT.as_slice())?;
    Some(Stored::Fingerprint(u64::from_le_bytes(
        rest.try_into().ok()?,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_names_index_reads_back() {
        let names = SegmentNames {
            counters: ["a".to_string(), "b".to_string()].into(),
            gauge_histograms: ["h".to_string()].into(),
            sampling_interval_ms: Some("100".into()),
            ..Default::default()
        };
        let index = encode(&names).unwrap();
        assert!(index.starts_with(NAMES));
        assert_eq!(read(Some(&index)), Some(Stored::Names(names)));
    }

    #[test]
    fn a_fingerprint_reads_back_and_anything_else_is_none() {
        let mut fp = FINGERPRINT.to_vec();
        fp.extend_from_slice(&7u64.to_le_bytes());
        assert_eq!(read(Some(&fp)), Some(Stored::Fingerprint(7)));
        assert_eq!(read(None), None);
        assert_eq!(read(Some(b"other")), None);
        assert_eq!(read(Some(b"MNI1not zstd")), None);
    }
}
