//! A sealed segment's names index, kept as dendro's per-segment caller index
//! (`SegmentRow::caller_index`: a row of `segment_indexes` in a schema 5
//! archive, the `segments.caller_index` column in a schema 4 one), so a
//! reader learns a table's metric names, and its first segment's interval,
//! from the catalog without fetching a segment.
//!
//! The writer stores a [`SegmentNames`]: the metric names the segment's
//! footer holds by kind, and the footer keys a probe reads when it opens the
//! segment (`sampling_interval_ms`, `metriken.format`, `metriken.layout`).
//! After it, in a second zstd frame, it stores the segment's
//! [`SegmentColumns`], from which a reader lists the table's label sets
//! without fetching its sealed segments. The open decodes only the first
//! frame. A third frame, written by metriken-storage after 0.14.0, holds the
//! segment's [`SegmentOpen`], with which a reader opens the table for a
//! query without fetching the segment. A reader that does not know the third
//! frame decompresses it with the second, parses the [`SegmentColumns`] at
//! the front and ignores the rest.
//! Segments sealed by metriken-storage 0.5.4 and earlier carry a names
//! fingerprint instead (or nothing), which a reader still uses: two segments
//! with the same fingerprint hold the same metric names, so the reader
//! parses the footer of one of them.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::long::OCCUPANT_LABEL;
use crate::segmented::{SegmentColumns, SegmentOpen};
use crate::{BufferPool, DataSource};

/// Prefix naming the index as a names fingerprint, version 1, so a
/// `caller_index` written by another producer is not mistaken for one.
const FINGERPRINT: &[u8; 4] = b"MNS1";

/// Prefix naming the index as a [`SegmentNames`], version 3: msgpack with
/// named fields, compressed with zstd, then optionally the segment's
/// [`SegmentColumns`] the same way, as a second zstd frame. A field added
/// later is left out by a reader that does not know it, and a field missing
/// is its default. A change to what a field means, including how a probe
/// classifies a column into the name sets or which footer keys of a
/// column become its labels (`Labels::from_metadata`, `STORAGE_KEYS`),
/// needs a new prefix, since a reader takes an index over the footer it was
/// built from. A reader fetches the segment of an `MNI` index whose version
/// it does not know.
///
/// Version 2 adds `overflow` and `overflow_bound` to `STORAGE_KEYS`
/// (metriken-storage 0.10.0), version 3 adds `scale` (0.11.0). A storage
/// key added after the release that first writes `MNI3` needs `MNI4`.
/// Version 1 and 2 indexes are still read ([`NAMES_V1`], [`NAMES_V2`]).
const NAMES: &[u8; 4] = b"MNI3";

/// Prefix of a version 2 index, written by metriken-storage 0.10.0: laid
/// out as version 3, its columns frame's labels taken with that release's
/// storage keys, and read as a version 1 index is (see [`NAMES_V1`]).
const NAMES_V2: &[u8; 4] = b"MNI2";

/// Prefix of a version 1 index, written by metriken-storage 0.9.0 and
/// earlier: laid out as version 3, with its columns frame's labels taken
/// with the storage keys of that release. Its label sets are read with every
/// key that is a storage key now removed ([`columns`]), which for the keys
/// version 1 knew removes nothing, so they match what this reader takes
/// from the segment's footer.
const NAMES_V1: &[u8; 4] = b"MNI1";

/// The prefix every names index version starts with.
const NAMES_FAMILY: &[u8; 3] = b"MNI";

/// The most a names index decompresses to before it is read as unreadable,
/// so a crafted index cannot exhaust memory at open. A [`SegmentNames`] of
/// 2500 names of 20 bytes each packs to about 53 KB (21 bytes a name).
const MAX_DECODED: u64 = 4 * 1024 * 1024;

/// The most a [`SegmentColumns`] frame decompresses to; past it, a listing
/// builds the table's reader instead.
const MAX_COLUMNS_DECODED: u64 = 64 * 1024 * 1024;

/// The most a [`SegmentOpen`] frame decompresses to; past it, the open
/// fetches the segment instead.
const MAX_OPEN_DECODED: u64 = 256 * 1024 * 1024;

/// What a segment's footer gives a reader at open: the metric names it
/// holds by kind, and the footer keys the open reads.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct SegmentNames {
    /// Metrics held in integer counter columns.
    pub counters: BTreeSet<String>,
    /// Metrics held in integer gauge columns.
    pub gauges: BTreeSet<String>,
    pub histograms: BTreeSet<String>,
    pub gauge_histograms: BTreeSet<String>,
    /// Metrics held in float counter columns, which a reader lists as
    /// counters. Only a format 3 segment has any, and a reader that does not
    /// know this field refuses that segment's `format` before it would list
    /// without them.
    pub float_counters: BTreeSet<String>,
    /// Metrics held in float gauge columns, listed as gauges.
    pub float_gauges: BTreeSet<String>,
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
    /// The names, and the segment's [`SegmentColumns`] frame, compressed
    /// ([`columns`] decodes it), when the index has one.
    Names(Box<SegmentNames>, Option<Frame>),
    /// A names fingerprint: the segment holds the same names as any other
    /// with this value.
    Fingerprint(u64),
    /// A names index that does not decode (written by a later version that
    /// changed a field's type, say): the segment's names are not known.
    Unreadable,
}

/// A names index's columns frame, compressed, and whether it is from an
/// index older than this version, whose label sets may hold keys that are
/// storage keys now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Frame {
    pub(crate) bytes: std::sync::Arc<[u8]>,
    pub(crate) older: bool,
    /// The [`SegmentOpen`] frame after it, compressed, when the index has
    /// one.
    pub(crate) open: Option<std::sync::Arc<[u8]>>,
}

/// The index to store with a sealed segment, or `None` when its footer
/// cannot be read (the segment is then read as unindexed). It holds no
/// columns frame when the segment holds one histogram under two configs,
/// which the segmented reader refuses.
pub(crate) fn index_of(segment: &[u8]) -> Option<Vec<u8>> {
    let (names, probe) = probe_of(segment)?;
    let columns = SegmentColumns::of(&probe).ok();
    let open = columns.as_ref().map(|c| SegmentOpen::of(&probe, c));
    encode(&names, columns.as_ref(), open.as_ref())
}

/// The names a probe of `segment` reads, as the reader's open does, or
/// `None` when its footer cannot be read, names one of the keys stored
/// more than once (the probe and this index could read different entries),
/// or has a `sampling_interval_ms` that is not a non-negative integer,
/// which the probe does not accept.
#[cfg(test)]
pub(crate) fn names_of(segment: &[u8]) -> Option<SegmentNames> {
    probe_of(segment).map(|(names, _)| names)
}

/// [`names_of`], with the probe it read.
fn probe_of(segment: &[u8]) -> Option<(SegmentNames, crate::parquet::MultiParquetSource)> {
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
    let names = SegmentNames {
        counters: probe.scalar_names(true, false).into_iter().collect(),
        gauges: probe.scalar_names(false, false).into_iter().collect(),
        histograms: probe.histogram_names().into_iter().collect(),
        gauge_histograms: probe.gauge_histogram_names().into_iter().collect(),
        float_counters: probe.scalar_names(true, true).into_iter().collect(),
        float_gauges: probe.scalar_names(false, true).into_iter().collect(),
        sampling_interval_ms: probe.metadata_get("sampling_interval_ms"),
        format: probe.metadata_get(crate::format::FORMAT_KEY),
        layout: probe.metadata_get(crate::long::LAYOUT_KEY),
    };
    Some((names, probe))
}

/// The index: the names, then the columns when there are any, then the
/// open parts when there are columns and parts.
fn encode(
    names: &SegmentNames,
    columns: Option<&SegmentColumns>,
    open: Option<&SegmentOpen>,
) -> Option<Vec<u8>> {
    let mut index = NAMES.to_vec();
    let packed = rmp_serde::to_vec_named(names).ok()?;
    index.extend(zstd::stream::encode_all(packed.as_slice(), 3).ok()?);
    if let Some(columns) = columns {
        let packed = rmp_serde::to_vec_named(columns).ok()?;
        index.extend(zstd::stream::encode_all(packed.as_slice(), 3).ok()?);
        if let Some(open) = open {
            let packed = rmp_serde::to_vec_named(&StoredOpen::from(open)).ok()?;
            index.extend(zstd::stream::encode_all(packed.as_slice(), 3).ok()?);
        }
    }
    Some(index)
}

/// What a stored index holds, or `None` for an index that is neither (none
/// at all, or another producer's).
pub(crate) fn read(index: Option<&[u8]>) -> Option<Stored> {
    let index = index?;
    let (rest, older) = if let Some(rest) = index.strip_prefix(NAMES.as_slice()) {
        (Some(rest), false)
    } else if let Some(rest) = index
        .strip_prefix(NAMES_V2.as_slice())
        .or_else(|| index.strip_prefix(NAMES_V1.as_slice()))
    {
        (Some(rest), true)
    } else {
        (None, false)
    };
    if let Some(rest) = rest {
        // The first frame is the names; what follows it, the columns.
        let names = zstd::zstd_safe::find_frame_compressed_size(rest)
            .ok()
            .and_then(|n| {
                let (names, columns) = rest.split_at(n);
                let names = decode(names, MAX_DECODED)?;
                let names: SegmentNames = rmp_serde::from_slice(&names).ok()?;
                // The columns frame, and the open frame after it when there
                // is one. An older version's index has no open frame.
                let frame = (!columns.is_empty()).then(|| {
                    let split = zstd::zstd_safe::find_frame_compressed_size(columns)
                        .ok()
                        .filter(|n| !older && *n < columns.len());
                    match split {
                        Some(n) => Frame {
                            bytes: columns[..n].into(),
                            older,
                            open: Some(columns[n..].into()),
                        },
                        None => Frame {
                            bytes: columns.into(),
                            older,
                            open: None,
                        },
                    }
                });
                Some((names, frame))
            });
        return Some(names.map_or(Stored::Unreadable, |(names, columns)| {
            Stored::Names(Box::new(names), columns)
        }));
    }
    if index.starts_with(NAMES_FAMILY) {
        return Some(Stored::Unreadable);
    }
    let rest = index.strip_prefix(FINGERPRINT.as_slice())?;
    Some(Stored::Fingerprint(u64::from_le_bytes(
        rest.try_into().ok()?,
    )))
}

/// A columns frame from [`Stored::Names`], decoded, or `None` when it does
/// not decode or decompresses past [`MAX_COLUMNS_DECODED`]. An older
/// version's frame's label sets lose every key that is a storage key now
/// (see [`NAMES_V1`]).
pub(crate) fn columns(frame: &Frame) -> Option<SegmentColumns> {
    let mut columns: SegmentColumns =
        rmp_serde::from_slice(&decode(&frame.bytes, MAX_COLUMNS_DECODED)?).ok()?;
    if frame.older {
        for list in [
            &mut columns.counters,
            &mut columns.gauges,
            &mut columns.histograms,
            &mut columns.gauge_histograms,
            &mut columns.float_counters,
            &mut columns.float_gauges,
        ] {
            for (_, labels) in list.iter_mut() {
                labels
                    .inner
                    .retain(|k, _| !crate::labels::is_storage_key(k));
            }
        }
    }
    Some(columns)
}

/// A segment's columns and open parts from its stored `index`, or `None`
/// when the index does not hold both (none, another producer's, an older
/// version's, or one that does not decode).
pub(crate) fn parts(index: &[u8]) -> Option<(SegmentColumns, SegmentOpen)> {
    let Some(Stored::Names(_, Some(frame))) = read(Some(index)) else {
        return None;
    };
    // The open parts first: a frame of another version is turned down
    // before the columns are decoded for nothing.
    let open = open_parts(&frame)?;
    Some((columns(&frame)?, open))
}

/// The open parts of a [`Stored::Names`] frame, decoded, or `None` when it
/// has none, they do not decode, or they decompress past
/// [`MAX_OPEN_DECODED`].
pub(crate) fn open_parts(frame: &Frame) -> Option<SegmentOpen> {
    let open = frame.open.as_ref()?;
    // The version first, from the frame's first bytes: a frame of another
    // version is neither decompressed nor parsed whole.
    if open_version(open)? != OPEN_VERSION {
        return None;
    }
    let stored: StoredOpen = rmp_serde::from_slice(&decode(open, MAX_OPEN_DECODED)?).ok()?;
    Some(stored.into())
}

/// The version of a compressed open frame, read from its first decompressed
/// bytes: [`StoredOpen`] is written as a map whose first entry is
/// `version`. `None` when the frame does not start that way (written without
/// a version, or not by this crate).
fn open_version(open: &[u8]) -> Option<u32> {
    use std::io::Read;
    let mut head = Vec::new();
    zstd::stream::Decoder::new(open)
        .ok()?
        .take(32)
        .read_to_end(&mut head)
        .ok()?;
    let mut at = match *head.first()? {
        0x80..=0x8f => 1,
        0xde => 3,
        0xdf => 5,
        _ => return None,
    };
    let key = b"version";
    if *head.get(at)? != 0xa0 | key.len() as u8 || head.get(at + 1..at + 1 + key.len())? != key {
        return None;
    }
    at += 1 + key.len();
    let value = |n: usize| -> Option<u64> {
        let bytes = head.get(at + 1..at + 1 + n)?;
        Some(bytes.iter().fold(0u64, |v, b| v << 8 | u64::from(*b)))
    };
    let version = match *head.get(at)? {
        b @ 0x00..=0x7f => u64::from(b),
        0xcc => value(1)?,
        0xcd => value(2)?,
        0xce => value(4)?,
        0xcf => value(8)?,
        _ => return None,
    };
    u32::try_from(version).ok()
}

type Labels = BTreeMap<String, String>;

/// A [`SegmentOpen`] as the index stores it. A long segment has a counter
/// column reference and a column map entry per occupant per column, which
/// differ only in the occupant; each run of those is stored once, with the
/// run's occupants. [`SegmentOpen`] is rebuilt exactly, in the same order.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct StoredOpen {
    /// [`OPEN_VERSION`] when written. A reader uses only the version it
    /// knows: a field added or changed later, under the same names index
    /// prefix, would otherwise be read as if it meant what it means here.
    /// Must stay the first field, named `version`: `open_version` reads it
    /// by position.
    version: u32,
    counter_refs: Vec<StoredRef>,
    column_map: Vec<(String, Vec<StoredColumn>)>,
    units: Vec<(String, Vec<Option<String>>)>,
    duplicated: Vec<(String, Labels)>,
    file_metadata: Labels,
    span: Option<(u64, u64)>,
    interval: f64,
}

/// Counter column references in order: one, or a run of one column's
/// occupants (its labels without the occupant label, its position without
/// the occupant).
#[derive(Debug, Serialize, Deserialize)]
enum StoredRef {
    One(String, Labels, crate::ColumnPosition),
    Occupants(String, Labels, crate::ColumnPosition, Vec<u64>),
}

/// A metric's column map entries: one, or one column's entries for several
/// occupants (its labels without the occupant label).
#[derive(Debug, Serialize, Deserialize)]
enum StoredColumn {
    One(Labels, String),
    Occupants(Labels, String, Vec<u64>),
}

/// `labels` without its occupant label, and that occupant, when the label
/// is the canonical form of `occupant` (or of any number, when `occupant` is
/// `None`), so that adding it back gives `labels` again.
fn split_occupant(labels: &Labels, occupant: Option<u64>) -> Option<(Labels, u64)> {
    let text = labels.get(OCCUPANT_LABEL)?;
    let id: u64 = text.parse().ok()?;
    if id.to_string() != *text || occupant.is_some_and(|o| o != id) {
        return None;
    }
    let mut base = labels.clone();
    base.remove(OCCUPANT_LABEL);
    Some((base, id))
}

fn with_occupant(base: &Labels, id: u64) -> Labels {
    let mut labels = base.clone();
    labels.insert(OCCUPANT_LABEL.to_string(), id.to_string());
    labels
}

/// The version of [`StoredOpen`] this crate writes and reads. A change to
/// what [`SegmentOpen::of`] computes, or to a field of [`StoredOpen`], needs
/// a new one.
const OPEN_VERSION: u32 = 1;

/// The bytes of a stored names index that a listing reads: its prefix, names
/// frame and columns frame, without the open frame after them, which holds
/// each segment's span and so differs between segments whose names are the
/// same. Segments with equal keys list the same.
pub(crate) fn listing_key(index: &[u8]) -> &[u8] {
    // Every version's prefix is as long as this one's.
    if !index.starts_with(NAMES_FAMILY) || index.len() < NAMES.len() {
        return index;
    }
    let rest = &index[NAMES.len()..];
    let Ok(names) = zstd::zstd_safe::find_frame_compressed_size(rest) else {
        return index;
    };
    let Ok(columns) = zstd::zstd_safe::find_frame_compressed_size(&rest[names..]) else {
        return index;
    };
    &index[..NAMES.len() + names + columns]
}

/// Whether `index` holds an open frame this crate does not know: of a later
/// version, one that does not decode, or followed by more bytes. A re-index
/// leaves such an index as it is, and replaces one of an earlier version.
#[cfg(any(feature = "write", test))]
pub(crate) fn has_unknown_open_frame(index: &[u8]) -> bool {
    let Some(Stored::Names(_, Some(frame))) = read(Some(index)) else {
        return false;
    };
    let Some(open) = frame.open.as_ref() else {
        return false;
    };
    let one = zstd::zstd_safe::find_frame_compressed_size(open).ok();
    if one != Some(open.len()) {
        return true;
    }
    if open_version(open).is_some_and(|v| v > OPEN_VERSION) {
        return true;
    }
    // Of this version or an earlier one: unknown only if it does not decode.
    decode(open, MAX_OPEN_DECODED)
        .and_then(|bytes| rmp_serde::from_slice::<StoredOpen>(&bytes).ok())
        .is_none()
}

impl From<&SegmentOpen> for StoredOpen {
    fn from(open: &SegmentOpen) -> Self {
        let mut counter_refs: Vec<StoredRef> = Vec::new();
        for (name, labels, position) in &open.counter_refs {
            let Some((base, id)) = position
                .occupant
                .and_then(|o| split_occupant(labels, Some(o)))
            else {
                counter_refs.push(StoredRef::One(name.clone(), labels.clone(), *position));
                continue;
            };
            let template = crate::ColumnPosition {
                occupant: None,
                ..*position
            };
            match counter_refs.last_mut() {
                Some(StoredRef::Occupants(n, b, p, ids))
                    if n == name && *b == base && *p == template =>
                {
                    ids.push(id)
                }
                _ => {
                    counter_refs.push(StoredRef::Occupants(name.clone(), base, template, vec![id]))
                }
            }
        }
        let column_map = open
            .column_map
            .iter()
            .map(|(metric, cols)| {
                let mut one = Vec::new();
                let mut grouped: BTreeMap<(Labels, String), Vec<u64>> = BTreeMap::new();
                for (labels, col) in cols {
                    match split_occupant(labels, None) {
                        Some((base, id)) => {
                            grouped.entry((base, col.clone())).or_default().push(id)
                        }
                        None => one.push(StoredColumn::One(labels.clone(), col.clone())),
                    }
                }
                one.extend(
                    grouped
                        .into_iter()
                        .map(|((base, col), ids)| StoredColumn::Occupants(base, col, ids)),
                );
                (metric.clone(), one)
            })
            .collect();
        Self {
            version: OPEN_VERSION,
            counter_refs,
            column_map,
            units: open.units.clone(),
            duplicated: open.duplicated.clone(),
            file_metadata: open.file_metadata.clone(),
            span: open.span,
            interval: open.interval,
        }
    }
}

impl From<StoredOpen> for SegmentOpen {
    fn from(stored: StoredOpen) -> Self {
        let mut counter_refs = Vec::new();
        for r in stored.counter_refs {
            match r {
                StoredRef::One(name, labels, position) => {
                    counter_refs.push((name, labels, position))
                }
                StoredRef::Occupants(name, base, template, ids) => {
                    for id in ids {
                        counter_refs.push((
                            name.clone(),
                            with_occupant(&base, id),
                            crate::ColumnPosition {
                                occupant: Some(id),
                                ..template
                            },
                        ));
                    }
                }
            }
        }
        let column_map = stored
            .column_map
            .into_iter()
            .map(|(metric, cols)| {
                let mut out: Vec<(Labels, String)> = Vec::new();
                for c in cols {
                    match c {
                        StoredColumn::One(labels, col) => out.push((labels, col)),
                        StoredColumn::Occupants(base, col, ids) => out.extend(
                            ids.into_iter()
                                .map(|id| (with_occupant(&base, id), col.clone())),
                        ),
                    }
                }
                out.sort();
                (metric, out)
            })
            .collect();
        Self {
            counter_refs,
            column_map,
            units: stored.units,
            duplicated: stored.duplicated,
            file_metadata: stored.file_metadata,
            span: stored.span,
            interval: stored.interval,
        }
    }
}

/// `compressed` decompressed, or `None` when it does not decode or exceeds
/// `max` bytes.
fn decode(compressed: &[u8], max: u64) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    zstd::stream::Decoder::new(compressed)
        .ok()?
        .take(max + 1)
        .read_to_end(&mut out)
        .ok()?;
    (out.len() as u64 <= max).then_some(out)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// A long segment's per-occupant references and column map entries
    /// are stored grouped and read back exactly, in order, next to ones
    /// that are not an occupant's and an occupant label that is not a
    /// canonical number.
    #[test]
    fn open_parts_read_back_exactly() {
        use crate::ColumnPosition;
        let pos = |col_idx: u32, occupant: Option<u64>| ColumnPosition {
            col_idx,
            begin_col: Some(1),
            width_col: Some(2),
            occupant,
            holds_values: true,
            float: false,
        };
        let labels = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let open = SegmentOpen {
            counter_refs: vec![
                (
                    "c".into(),
                    labels(&[("op", "a"), (OCCUPANT_LABEL, "0")]),
                    pos(5, Some(0)),
                ),
                (
                    "c".into(),
                    labels(&[("op", "a"), (OCCUPANT_LABEL, "7")]),
                    pos(5, Some(7)),
                ),
                (
                    "c".into(),
                    labels(&[("op", "b"), (OCCUPANT_LABEL, "0")]),
                    pos(6, Some(0)),
                ),
                ("w".into(), labels(&[("cpu", "1")]), pos(9, None)),
                (
                    "c".into(),
                    labels(&[("op", "a"), (OCCUPANT_LABEL, "9")]),
                    pos(5, Some(9)),
                ),
                // Not canonical: stored as it is.
                (
                    "c".into(),
                    labels(&[("op", "a"), (OCCUPANT_LABEL, "07")]),
                    pos(5, Some(7)),
                ),
                // One series in two columns (it turned float): two runs, one
                // name and label set, two positions.
                (
                    "c".into(),
                    labels(&[("op", "f"), (OCCUPANT_LABEL, "1")]),
                    pos(11, Some(1)),
                ),
                (
                    "c".into(),
                    labels(&[("op", "f"), (OCCUPANT_LABEL, "2")]),
                    ColumnPosition {
                        float: true,
                        ..pos(12, Some(2))
                    },
                ),
            ],
            column_map: vec![(
                "c".into(),
                vec![
                    // Sorted, as `SegmentOpen::of` writes it.
                    (labels(&[("op", "a"), (OCCUPANT_LABEL, "0")]), "c#5".into()),
                    (labels(&[("op", "b"), (OCCUPANT_LABEL, "0")]), "c#6".into()),
                    (labels(&[("op", "a"), (OCCUPANT_LABEL, "7")]), "c#5".into()),
                    (labels(&[("op", "z")]), "c#9".into()),
                ],
            )],
            units: vec![("c".into(), vec![Some("count".into())])],
            duplicated: vec![("c".into(), labels(&[("op", "a")]))],
            file_metadata: labels(&[("sampling_interval_ms", "100")]),
            span: Some((1, 2)),
            interval: 0.1,
        };
        let stored = StoredOpen::from(&open);
        assert_eq!(stored.counter_refs.len(), 7, "{:?}", stored.counter_refs);
        let packed = rmp_serde::to_vec_named(&stored).unwrap();
        let back: SegmentOpen = rmp_serde::from_slice::<StoredOpen>(&packed).unwrap().into();
        assert_eq!(back, open);

        // Stored in an index, the open parts read back; a reader before
        // them, which decodes the columns frame with the open frame after it,
        // reads the columns; a listing keys the index without them.
        let names = SegmentNames::default();
        let columns = SegmentColumns {
            counters: vec![("c".into(), crate::labels::Labels::from([("op", "a")]))],
            ..Default::default()
        };
        let index = encode(&names, Some(&columns), Some(&open)).unwrap();
        assert_eq!(parts(&index), Some((columns.clone(), open.clone())));
        let Some(Stored::Names(_, Some(frame))) = read(Some(&index)) else {
            panic!("an index with columns");
        };
        let mut older = frame.bytes.to_vec();
        older.extend_from_slice(frame.open.as_deref().unwrap());
        let older = Frame {
            bytes: older.into(),
            older: false,
            open: None,
        };
        assert_eq!(super::columns(&older), Some(columns.clone()));
        let without = encode(&names, Some(&columns), None).unwrap();
        assert_eq!(listing_key(&index), without.as_slice());
        assert!(!has_unknown_open_frame(&index));
    }

    /// An open frame of another version is not read. A re-index leaves one
    /// of a later version, or with a frame after it, and replaces one of an
    /// earlier version.
    #[test]
    fn an_open_frame_this_version_does_not_know_is_left() {
        let names = SegmentNames::default();
        let columns = SegmentColumns::default();
        let mut later = StoredOpen::from(&SegmentOpen::default());
        later.version = OPEN_VERSION + 1;
        let mut index = encode(&names, Some(&columns), None).unwrap();
        let packed = rmp_serde::to_vec_named(&later).unwrap();
        index.extend(zstd::stream::encode_all(packed.as_slice(), 3).unwrap());
        assert_eq!(parts(&index), None);
        assert!(has_unknown_open_frame(&index));

        // An earlier version's frame is not read, and a re-index replaces it.
        let mut earlier = StoredOpen::from(&SegmentOpen::default());
        earlier.version = OPEN_VERSION - 1;
        let mut index = encode(&names, Some(&columns), None).unwrap();
        let packed = rmp_serde::to_vec_named(&earlier).unwrap();
        index.extend(zstd::stream::encode_all(packed.as_slice(), 3).unwrap());
        assert_eq!(parts(&index), None);
        assert!(!has_unknown_open_frame(&index));

        let mut extra = encode(&names, Some(&columns), Some(&SegmentOpen::default())).unwrap();
        assert!(!has_unknown_open_frame(&extra));
        extra.extend(zstd::stream::encode_all(&b"later"[..], 3).unwrap());
        assert!(has_unknown_open_frame(&extra));
    }

    #[test]
    fn a_names_index_reads_back() {
        let names = SegmentNames {
            counters: ["a".to_string(), "b".to_string()].into(),
            gauge_histograms: ["h".to_string()].into(),
            sampling_interval_ms: Some("100".into()),
            ..Default::default()
        };
        let index = encode(&names, None, None).unwrap();
        assert!(index.starts_with(NAMES));
        assert_eq!(
            read(Some(&index)),
            Some(Stored::Names(Box::new(names.clone()), None))
        );

        let columns = SegmentColumns {
            counters: vec![(
                "a".into(),
                crate::Labels::from(std::collections::BTreeMap::from([(
                    "k".to_string(),
                    "v".to_string(),
                )])),
            )],
            histogram_configs: [("h".to_string(), (4, 32))].into(),
            ..Default::default()
        };
        let index = encode(&names, Some(&columns), None).unwrap();
        let Some(Stored::Names(read_names, Some(frame))) = read(Some(&index)) else {
            panic!("no columns frame");
        };
        assert_eq!(*read_names, names);
        assert_eq!(super::columns(&frame), Some(columns));
    }

    /// A version 1 or 2 index reads, and its columns frame's label sets
    /// lose the keys that became storage keys since (`overflow` and
    /// `overflow_bound` in version 2, `scale` in version 3), as the
    /// segment's footer read now would; a version 3 frame's label sets are
    /// read as stored.
    #[test]
    fn an_older_index_reads_without_the_new_storage_keys() {
        let names = SegmentNames {
            histograms: ["h".to_string()].into(),
            ..Default::default()
        };
        let labels = |pairs: &[(&str, &str)]| {
            crate::Labels::from(
                pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect::<std::collections::BTreeMap<_, _>>(),
            )
        };
        let columns = SegmentColumns {
            histograms: vec![(
                "h".into(),
                labels(&[
                    ("op", "r"),
                    ("overflow", "x"),
                    ("overflow_bound", "1"),
                    ("scale", "10"),
                ]),
            )],
            histogram_configs: [("h".to_string(), (4, 32))].into(),
            ..Default::default()
        };
        let mut index = encode(&names, Some(&columns), None).unwrap();
        let Some(Stored::Names(_, Some(frame))) = read(Some(&index)) else {
            panic!("no columns frame");
        };
        assert!(!frame.older);
        assert_eq!(super::columns(&frame), Some(columns.clone()));

        for prefix in [NAMES_V1, NAMES_V2] {
            index[..4].copy_from_slice(prefix);
            let Some(Stored::Names(read_names, Some(frame))) = read(Some(&index)) else {
                panic!("no columns frame");
            };
            assert_eq!(*read_names, names);
            assert!(frame.older);
            let read = super::columns(&frame).unwrap();
            assert_eq!(read.histograms, vec![("h".into(), labels(&[("op", "r")]))]);
        }
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
        assert_eq!(
            read(Some(&index)),
            Some(Stored::Names(Box::new(expected), None))
        );
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

    /// An index decompresses up to the cap and no further.
    #[test]
    fn an_index_decodes_up_to_the_cap() {
        let z = |n: usize| zstd::stream::encode_all(vec![0u8; n].as_slice(), 3).unwrap();
        assert_eq!(
            decode(&z(MAX_DECODED as usize), MAX_DECODED).map(|v| v.len()),
            Some(MAX_DECODED as usize)
        );
        assert_eq!(decode(&z(MAX_DECODED as usize + 1), MAX_DECODED), None);
    }

    #[test]
    fn a_fingerprint_reads_back_and_anything_else_is_none() {
        let mut fp = FINGERPRINT.to_vec();
        fp.extend_from_slice(&7u64.to_le_bytes());
        assert_eq!(read(Some(&fp)), Some(Stored::Fingerprint(7)));
        assert_eq!(read(None), None);
        assert_eq!(read(Some(b"other")), None);
        assert_eq!(read(Some(b"MNI1not zstd")), Some(Stored::Unreadable));
        assert_eq!(read(Some(b"MNI4a later index")), Some(Stored::Unreadable));
    }
}
