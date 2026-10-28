//! A sealed segment's names index, kept as dendro's per-segment caller index
//! (`SegmentRow::caller_index`: a row of `segment_indexes` in a schema 5
//! archive, the `segments.caller_index` column in a schema 4 one), so a
//! reader learns a table's metric names, and its first segment's interval,
//! from the catalog without fetching a segment.
//!
//! The writer stores a [`SegmentNames`]: the metric names the segment's
//! footer holds by kind, and the footer keys a probe reads when it opens the
//! segment (`sampling_interval_ms`, `metriken.format`, `metriken.layout`).
//! Segments sealed before it carry a names fingerprint instead (or nothing),
//! which a reader still uses: two segments with the same fingerprint hold
//! the same metric names, so the reader parses the footer of one of them.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{BufferPool, DataSource};

/// Prefix naming the index as a names fingerprint, version 1, so a
/// `caller_index` written by another producer is not mistaken for one.
const FINGERPRINT: &[u8; 4] = b"MNS1";

/// Prefix naming the index as a [`SegmentNames`], version 1: msgpack with
/// named fields, compressed with zstd. A field added later is left out by a
/// reader that does not know it, and a field missing is its default. A
/// change to what a field means, including how a probe classifies a column
/// into the four name sets, needs a new prefix, since a reader takes an
/// index over the footer it was built from.
const NAMES: &[u8; 4] = b"MNI1";

/// What a segment's footer gives a reader at open: the metric names it
/// holds by kind, and the footer keys the open reads.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct SegmentNames {
    pub counters: BTreeSet<String>,
    pub gauges: BTreeSet<String>,
    pub histograms: BTreeSet<String>,
    pub gauge_histograms: BTreeSet<String>,
    /// The footer's `sampling_interval_ms`, as written.
    pub sampling_interval_ms: Option<String>,
    /// The footer's `metriken.format`, as written.
    pub format: Option<String>,
    /// The footer's `metriken.layout`, as written.
    pub layout: Option<String>,
}

/// A stored index, as a reader uses it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Stored {
    Names(SegmentNames),
    /// A names fingerprint: the segment holds the same names as any other
    /// with this value.
    Fingerprint(u64),
    /// A names index that does not decode (written by a later version that
    /// changed a field's type, say): the segment's names are not known.
    Unreadable,
}

/// The index to store with a sealed segment, or `None` when its footer
/// cannot be read (the segment is then read as unindexed).
pub(crate) fn index_of(segment: &[u8]) -> Option<Vec<u8>> {
    encode(&names_of(segment)?)
}

/// The names a probe of `segment` reads, as the reader's open does, or
/// `None` when its footer cannot be read, names one of the keys stored
/// more than once (the probe and this index could read different entries),
/// or has a `sampling_interval_ms` that is not a non-negative integer,
/// which the probe does not accept.
pub(crate) fn names_of(segment: &[u8]) -> Option<SegmentNames> {
    let bytes = bytes::Bytes::copy_from_slice(segment);
    let meta = parquet::arrow::arrow_reader::ArrowReaderMetadata::load(
        &bytes,
        parquet::arrow::arrow_reader::ArrowReaderOptions::default(),
    )
    .ok()?;
    let kv = meta.metadata().file_metadata().key_value_metadata();
    let entries = |key: &str| -> Vec<&str> {
        kv.into_iter()
            .flatten()
            .filter(|e| e.key == key)
            .map(|e| e.value.as_deref().unwrap_or(""))
            .collect()
    };
    for key in [crate::format::FORMAT_KEY, crate::long::LAYOUT_KEY] {
        if entries(key).len() > 1 {
            return None;
        }
    }
    match entries("sampling_interval_ms")[..] {
        [] => {}
        [interval] => {
            interval.parse::<u64>().ok()?;
        }
        _ => return None,
    }
    // A footer-only read: no page is decoded, so the pool stays empty.
    let probe = crate::parquet::MultiParquetSource::open_bytes_with_pool(
        bytes,
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
        layout: probe.metadata_get(crate::long::LAYOUT_KEY),
    })
}

fn encode(names: &SegmentNames) -> Option<Vec<u8>> {
    let packed = rmp_serde::to_vec_named(names).ok()?;
    let mut index = NAMES.to_vec();
    index.extend(zstd::stream::encode_all(packed.as_slice(), 3).ok()?);
    Some(index)
}

/// What a stored index holds, or `None` for an index that is neither (none
/// at all, or another producer's).
pub(crate) fn read(index: Option<&[u8]>) -> Option<Stored> {
    let index = index?;
    if let Some(rest) = index.strip_prefix(NAMES.as_slice()) {
        let names = zstd::stream::decode_all(rest)
            .ok()
            .and_then(|packed| rmp_serde::from_slice(&packed).ok());
        return Some(names.map_or(Stored::Unreadable, Stored::Names));
    }
    let rest = index.strip_prefix(FINGERPRINT.as_slice())?;
    Some(Stored::Fingerprint(u64::from_le_bytes(
        rest.try_into().ok()?,
    )))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

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

    /// A field this reader does not know is left out, and one it knows but
    /// the index lacks is its default.
    #[test]
    fn a_names_index_with_other_fields_reads() {
        #[derive(Serialize)]
        struct Later {
            gauges: BTreeSet<String>,
            labels: Vec<String>,
        }
        let later = Later {
            gauges: ["g".to_string()].into(),
            labels: vec!["k".into()],
        };
        let mut index = NAMES.to_vec();
        index.extend(
            zstd::stream::encode_all(rmp_serde::to_vec_named(&later).unwrap().as_slice(), 3)
                .unwrap(),
        );
        let expected = SegmentNames {
            gauges: later.gauges,
            ..Default::default()
        };
        assert_eq!(read(Some(&index)), Some(Stored::Names(expected)));
    }

    /// A parquet segment with one timestamp column and `interval` as its
    /// footer's `sampling_interval_ms`.
    fn segment(interval: &str) -> Vec<u8> {
        segment_with(&[("sampling_interval_ms", interval)])
    }

    /// A parquet segment with one timestamp column and `kv` as its footer's
    /// key-value metadata.
    fn segment_with(kv: &[(&str, &str)]) -> Vec<u8> {
        use arrow::array::UInt64Array;
        use arrow::datatypes::{DataType, Field, Schema};
        use arrow::record_batch::RecordBatch;
        use parquet::file::metadata::KeyValue;
        use parquet::file::properties::WriterProperties;

        let schema = Arc::new(Schema::new(vec![Field::new(
            "timestamp",
            DataType::UInt64,
            false,
        )]));
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![Arc::new(UInt64Array::from(vec![1]))],
        )
        .unwrap();
        let props = WriterProperties::builder()
            .set_key_value_metadata(Some(
                kv.iter()
                    .map(|(k, v)| KeyValue::new(k.to_string(), v.to_string()))
                    .collect(),
            ))
            .build();
        let mut out = Vec::new();
        let mut writer =
            parquet::arrow::ArrowWriter::try_new(&mut out, schema, Some(props)).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        out
    }

    /// A segment whose `sampling_interval_ms` the probe would not accept
    /// gets no names index, rather than a panic in the probe.
    #[test]
    fn a_segment_with_a_bad_interval_gets_no_names_index() {
        let names = names_of(&segment("100")).unwrap();
        assert_eq!(names.sampling_interval_ms.as_deref(), Some("100"));
        assert_eq!(names_of(&segment("1.5")), None);
        assert_eq!(names_of(&segment("")), None);
    }

    /// A footer naming a key the index stores more than once gets no names
    /// index: the probe and the index could read different entries.
    #[test]
    fn a_segment_with_a_repeated_key_gets_no_names_index() {
        let interval = "sampling_interval_ms";
        assert_eq!(
            names_of(&segment_with(&[(interval, "100"), (interval, "200")])),
            None
        );
        let format = crate::format::FORMAT_KEY;
        assert_eq!(
            names_of(&segment_with(&[(format, "1"), (format, "1")])),
            None
        );
        let layout = crate::long::LAYOUT_KEY;
        assert_eq!(
            names_of(&segment_with(&[(layout, "long"), (layout, "long")])),
            None
        );
        assert!(names_of(&segment_with(&[(format, "1"), (layout, "long")])).is_some());
    }

    #[test]
    fn a_fingerprint_reads_back_and_anything_else_is_none() {
        let mut fp = FINGERPRINT.to_vec();
        fp.extend_from_slice(&7u64.to_le_bytes());
        assert_eq!(read(Some(&fp)), Some(Stored::Fingerprint(7)));
        assert_eq!(read(None), None);
        assert_eq!(read(Some(b"other")), None);
        assert_eq!(read(Some(b"MNI1not zstd")), Some(Stored::Unreadable));
    }
}
