//! The long segment layout: one row per (timestamp, occupant), one column per
//! metric. See `docs/journal/2026-09-28-long-segments.md`.
//!
//! The format only; `metriken-query` reads it.
//!
//! A writer marks a segment long with [`LAYOUT_KEY`] = [`LAYOUT_LONG`] in the
//! file key-value metadata, adds a `UInt64` column named [`OCCUPANT_COLUMN`],
//! and lists the occupants the segment holds under [`OCCUPANTS_KEY`], encoded
//! with [`encode_occupant_ranges`]. The reader presents each metric column and
//! occupant as one series labelled [`OCCUPANT_LABEL`].
//!
//! A metric column that has values for only some of those occupants lists
//! the ones it has under [`column_occupants_key`], in the same encoding, and
//! the reader presents only those. A column without the key has values for
//! every occupant the segment lists, or was written before the key existed
//! (metriken-storage 0.2.0 and earlier); either way the reader presents it
//! once per occupant in [`OCCUPANTS_KEY`].

/// File key-value metadata key naming the segment layout.
pub const LAYOUT_KEY: &str = "metriken.layout";

/// [`LAYOUT_KEY`]'s value for a long segment.
pub const LAYOUT_LONG: &str = "long";

/// File key-value metadata key listing a long segment's occupants.
pub const OCCUPANTS_KEY: &str = "metriken.occupants";

/// Prefix of the file key-value metadata key listing the occupants one
/// metric column has values for; see [`column_occupants_key`].
pub const COLUMN_OCCUPANTS_PREFIX: &str = "metriken.column_occupants.";

/// File key-value metadata key listing the occupants the column named
/// `column` (its name in the segment, `<metric>:buckets` for a histogram)
/// has a non-null value for, encoded with [`encode_occupant_ranges`].
///
/// File metadata rather than the column's field metadata: a reader before
/// this key existed turns every field metadata key it does not know into a
/// label, and the list differs between segments, so it would split each
/// series at every segment boundary. Unknown file keys are ignored.
pub fn column_occupants_key(column: &str) -> String {
    format!("{COLUMN_OCCUPANTS_PREFIX}{column}")
}

/// The column holding each row's occupant number.
pub const OCCUPANT_COLUMN: &str = "occupant";

/// The internal label a long segment's series carry: the occupant number.
/// Internal by the `__` rule, so consumers hide it; a reader supplies the
/// occupant's labels from its [occupant stream](crate::occupants).
pub const OCCUPANT_LABEL: &str = "__occupant__";

/// Encode a set of occupant numbers as ascending ranges: `0-1520,1523`.
///
/// Input need not be sorted or distinct.
pub fn encode_occupant_ranges(occupants: impl IntoIterator<Item = u64>) -> String {
    let mut v: Vec<u64> = occupants.into_iter().collect();
    v.sort_unstable();
    v.dedup();
    let mut out = String::new();
    let mut i = 0;
    while i < v.len() {
        let start = v[i];
        let mut end = start;
        while i + 1 < v.len() && v[i + 1] == end + 1 {
            end += 1;
            i += 1;
        }
        if !out.is_empty() {
            out.push(',');
        }
        if start == end {
            out.push_str(&start.to_string());
        } else {
            out.push_str(&format!("{start}-{end}"));
        }
        i += 1;
    }
    out
}

/// Decode [`encode_occupant_ranges`]' output, refusing more than `limit`
/// occupants: a segment cannot hold more occupants than it has rows, so a
/// list that says otherwise is malformed, and the limit keeps a bad range
/// from allocating without bound.
pub fn decode_occupant_ranges(s: &str, limit: u64) -> Result<Vec<u64>, String> {
    let mut out = Vec::new();
    if s.is_empty() {
        return Ok(out);
    }
    for part in s.split(',') {
        let (start, end) = match part.split_once('-') {
            Some((a, b)) => (parse(a)?, parse(b)?),
            None => {
                let n = parse(part)?;
                (n, n)
            }
        };
        if end < start {
            return Err(format!("occupant range {part} runs backwards"));
        }
        if let Some(&last) = out.last() {
            if start <= last {
                return Err(format!("occupant range {part} is not ascending"));
            }
        }
        if (end - start)
            .saturating_add(1)
            .saturating_add(out.len() as u64)
            > limit
        {
            return Err(format!("occupant list names more than {limit} occupants"));
        }
        out.extend(start..=end);
    }
    Ok(out)
}

fn parse(s: &str) -> Result<u64, String> {
    s.parse()
        .map_err(|_| format!("occupant number {s:?} is not an integer"))
}

/// One segment's file metadata, as [`merge_segment_metadata`] reads it.
#[derive(Debug, Clone, Copy)]
pub struct SegmentMetadata<'a> {
    /// The segment's file key-value metadata, without Arrow's schema entry
    /// (`ARROW:schema`).
    pub key_value: &'a [parquet::file::metadata::KeyValue],
    /// The segment's columns. A column in none of a segment's fields
    /// contributes no occupants for that segment, whatever its lists say.
    pub fields: &'a arrow::datatypes::Fields,
    /// The rows in the segment, the bound on how many occupants its lists
    /// may name, as when reading it.
    pub rows: u64,
}

/// The file key-value metadata of the segment that concatenating `inputs`
/// in order produces, or `None` when they cannot be merged.
///
/// For a compaction's metadata merge: a long segment's occupant lists change
/// whenever the population does, and merging only segments with identical
/// metadata leaves a long stream nearly as it was sealed.
///
/// Long segments merge exactly by union. The merged [`OCCUPANTS_KEY`] is the
/// union of the inputs' lists. A column's list ([`column_occupants_key`]) is
/// the union over the inputs that have the column, each contributing its own
/// list for the column or, without one, its segment list; it is omitted
/// when that union is the merged segment list, as the writer omits it.
/// Every other key, [`LAYOUT_KEY`] and the format version among them, must
/// have the same value in every input.
///
/// Segments that are not all long merge only when their metadata is
/// identical, entry for entry. `None` as well for a long segment with no
/// segment list, a list that does not decode within its segment's rows, or a
/// key given twice; this never panics on a malformed list.
pub fn merge_segment_metadata(
    inputs: &[SegmentMetadata<'_>],
) -> Option<Vec<parquet::file::metadata::KeyValue>> {
    use parquet::file::metadata::KeyValue;

    let first = inputs.first()?;
    let long = |kv: &[KeyValue]| {
        kv.iter()
            .any(|e| e.key == LAYOUT_KEY && e.value.as_deref() == Some(LAYOUT_LONG))
    };
    if !inputs.iter().all(|i| long(i.key_value)) {
        return inputs
            .iter()
            .all(|i| i.key_value == first.key_value)
            .then(|| first.key_value.to_vec());
    }

    // Each input's segment list, its column lists, and the rest of its
    // keys, which must agree.
    let mut all: BTreeSet<u64> = BTreeSet::new();
    let mut per_input = Vec::new();
    let mut rest: Option<BTreeMap<&str, Option<&str>>> = None;
    for input in inputs {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut occupants = None;
        let mut columns = HashMap::new();
        let mut others = BTreeMap::new();
        for e in input.key_value {
            if !seen.insert(e.key.as_str()) {
                return None;
            }
            if e.key == OCCUPANTS_KEY {
                occupants = Some(decode_occupant_ranges(e.value.as_deref()?, input.rows).ok()?);
            } else if let Some(column) = e.key.strip_prefix(COLUMN_OCCUPANTS_PREFIX) {
                let list = decode_occupant_ranges(e.value.as_deref()?, input.rows).ok()?;
                columns.insert(column, list);
            } else {
                others.insert(e.key.as_str(), e.value.as_deref());
            }
        }
        match &rest {
            None => rest = Some(others),
            Some(r) if *r == others => {}
            Some(_) => return None,
        }
        let occupants = occupants?;
        all.extend(&occupants);
        per_input.push((occupants, columns));
    }
    let all_encoded = encode_occupant_ranges(all.iter().copied());

    // The merged segment's columns, in the order they first appear.
    let mut order: Vec<&str> = Vec::new();
    for input in inputs {
        for f in input.fields.iter() {
            if !order.contains(&f.name().as_str()) {
                order.push(f.name());
            }
        }
    }
    let mut column_keys = Vec::new();
    for column in order {
        let mut has: BTreeSet<u64> = BTreeSet::new();
        for (input, (occupants, columns)) in inputs.iter().zip(&per_input) {
            if input.fields.find(column).is_some() {
                has.extend(columns.get(column).unwrap_or(occupants));
            }
        }
        let encoded = encode_occupant_ranges(has);
        if encoded != all_encoded {
            column_keys.push(KeyValue::new(column_occupants_key(column), encoded));
        }
    }

    // The first input's keys in its order, with the merged segment list in
    // place of its own and the column lists after them.
    let mut out: Vec<KeyValue> = first
        .key_value
        .iter()
        .filter(|e| !e.key.starts_with(COLUMN_OCCUPANTS_PREFIX))
        .map(|e| {
            if e.key == OCCUPANTS_KEY {
                KeyValue::new(OCCUPANTS_KEY.to_string(), all_encoded.clone())
            } else {
                e.clone()
            }
        })
        .collect();
    out.extend(column_keys);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_round_trip() {
        let occ = [5, 0, 1, 2, 3, 9, 7, 8, 3];
        let s = encode_occupant_ranges(occ);
        assert_eq!(s, "0-3,5,7-9");
        assert_eq!(
            decode_occupant_ranges(&s, 100).unwrap(),
            vec![0, 1, 2, 3, 5, 7, 8, 9]
        );
        assert_eq!(encode_occupant_ranges([]), "");
        assert!(decode_occupant_ranges("", 0).unwrap().is_empty());
    }

    #[test]
    fn malformed_lists_are_refused() {
        assert!(decode_occupant_ranges("0-18446744073709551615", 1000).is_err());
        assert!(decode_occupant_ranges("5-3", 10).is_err());
        assert!(decode_occupant_ranges("5,3", 10).is_err());
        assert!(decode_occupant_ranges("3,3", 10).is_err());
        assert!(decode_occupant_ranges("x", 10).is_err());
        assert!(decode_occupant_ranges("0-9", 9).is_err());
        assert_eq!(decode_occupant_ranges("0-9", 10).unwrap().len(), 10);
    }
    mod merge {
        use super::super::*;
        use arrow::datatypes::{DataType, Field, Fields};
        use parquet::file::metadata::KeyValue;

        fn fields(names: &[&str]) -> Fields {
            names
                .iter()
                .map(|n| Field::new(*n, DataType::UInt64, true))
                .collect()
        }

        fn kv(pairs: &[(&str, &str)]) -> Vec<KeyValue> {
            pairs
                .iter()
                .map(|(k, v)| KeyValue::new(k.to_string(), v.to_string()))
                .collect()
        }

        fn long(occupants: &str, columns: &[(&str, &str)]) -> Vec<KeyValue> {
            let mut out = kv(&[
                (LAYOUT_KEY, LAYOUT_LONG),
                (OCCUPANTS_KEY, occupants),
                ("metriken.format", "1"),
            ]);
            for (c, l) in columns {
                out.push(KeyValue::new(column_occupants_key(c), l.to_string()));
            }
            out
        }

        fn merge(inputs: &[(&[KeyValue], &Fields, u64)]) -> Option<Vec<(String, String)>> {
            let inputs: Vec<SegmentMetadata<'_>> = inputs
                .iter()
                .map(|(key_value, fields, rows)| SegmentMetadata {
                    key_value,
                    fields,
                    rows: *rows,
                })
                .collect();
            merge_segment_metadata(&inputs).map(|kv| {
                kv.into_iter()
                    .map(|e| (e.key, e.value.unwrap_or_default()))
                    .collect()
            })
        }

        fn pairs(kv: &[KeyValue]) -> Vec<(String, String)> {
            kv.iter()
                .map(|e| (e.key.clone(), e.value.clone().unwrap_or_default()))
                .collect()
        }

        /// The population churns between segments: the segment lists union,
        /// a column without a list contributes its segment's, and a column
        /// list that comes to equal the merged segment list is dropped.
        #[test]
        fn churn_merges_by_union() {
            let f = fields(&["timestamp", "occupant", "a", "b"]);
            // `a` has every occupant of the first segment, `b` only 0.
            let one = long("0-2", &[("b", "0")]);
            // `a` has 1 and 2, `b` every occupant of the second segment.
            let two = long("1-3", &[("a", "1-2")]);
            let merged = merge(&[(&one, &f, 9), (&two, &f, 9)]).unwrap();
            let mut want = long("0-3", &[("a", "0-2")]);
            want.retain(|e| !e.key.ends_with(".b"));
            assert_eq!(merged, pairs(&want));
        }

        /// A column a segment lacks contributes nothing for that segment;
        /// one it has without a list contributes the segment's list.
        #[test]
        fn an_absent_key_expands_and_an_absent_column_does_not() {
            let with_c = fields(&["timestamp", "occupant", "c"]);
            let without_c = fields(&["timestamp", "occupant"]);
            let one = long("0-2", &[]);
            let two = long("3-4", &[]);
            assert_eq!(
                merge(&[(&one, &with_c, 9), (&two, &without_c, 9)]).unwrap(),
                pairs(&long("0-4", &[("c", "0-2")]))
            );
            assert_eq!(
                merge(&[(&one, &with_c, 9), (&two, &with_c, 9)]).unwrap(),
                pairs(&long("0-4", &[])),
                "both have `c` for every occupant, so it needs no list"
            );
        }

        #[test]
        fn a_differing_key_refuses_the_merge() {
            let f = fields(&["timestamp", "occupant", "a"]);
            let one = long("0-2", &[]);
            let mut newer = long("0-2", &[]);
            newer[2].value = Some("2".to_string());
            assert_eq!(merge(&[(&one, &f, 9), (&newer, &f, 9)]), None);
            let mut unknown = long("0-2", &[]);
            unknown.push(KeyValue::new("x".to_string(), "y".to_string()));
            assert_eq!(merge(&[(&one, &f, 9), (&unknown, &f, 9)]), None);
            let wide = kv(&[("metriken.format", "1")]);
            assert_eq!(merge(&[(&one, &f, 9), (&wide, &f, 9)]), None);
        }

        #[test]
        fn segments_not_long_merge_only_when_identical() {
            let f = fields(&["timestamp", "a"]);
            let one = kv(&[("metriken.format", "1"), ("k", "v")]);
            assert_eq!(merge(&[(&one, &f, 9), (&one, &f, 9)]).unwrap(), pairs(&one));
            let two = kv(&[("metriken.format", "1"), ("k", "w")]);
            assert_eq!(merge(&[(&one, &f, 9), (&two, &f, 9)]), None);
        }

        #[test]
        fn a_malformed_list_refuses_the_merge() {
            let f = fields(&["timestamp", "occupant", "a"]);
            let good = long("0-2", &[]);
            for bad in [
                long("2-0", &[]),
                long("0-2", &[("a", "x")]),
                // More occupants than the segment has rows.
                long("0-20", &[]),
                long("0-2", &[("a", "0-20")]),
                // No segment list at all.
                kv(&[(LAYOUT_KEY, LAYOUT_LONG), ("metriken.format", "1")]),
                // A key given twice.
                [long("0-2", &[]), kv(&[(OCCUPANTS_KEY, "0")])].concat(),
            ] {
                assert_eq!(merge(&[(&good, &f, 9), (&bad, &f, 9)]), None, "{bad:?}");
                assert_eq!(merge(&[(&bad, &f, 9), (&good, &f, 9)]), None, "{bad:?}");
            }
            let mut none = long("0-2", &[]);
            none[1].value = None;
            assert_eq!(merge(&[(&none, &f, 9)]), None);
        }
    }
}

// ─── Occupant labels: the reader's relabel of a long table ─────────────────

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::labels::Labels;
use crate::segmented::{ColumnRelabel, Run};

/// Puts each occupant's labels on its series: a long table's series carry
/// only [`OCCUPANT_LABEL`] and the metric column's fixed labels, and this is
/// metriken-query's hook for the rest.
///
/// An occupant never changes labels, so a series is one run. The occupant
/// number stays on the series: two occupants can share every other label
/// (a recording with no `__uid__`), and it keeps them apart. It is internal
/// by the `__` rule, so listings and legends hide it.
pub struct OccupantLabels {
    labels: HashMap<u64, BTreeMap<String, String>>,
    /// Every key in the rows this was built from. A query filter on one of these cannot
    /// be answered by the columns, so it is taken off the segment filter and
    /// applied to the relabelled series.
    keys: BTreeSet<String>,
}

impl OccupantLabels {
    /// From an occupant stream's rows. An occupant restated with the same
    /// labels is one entry; restated with different ones is a defect of the
    /// writer, and the first labels win.
    pub fn new(rows: impl IntoIterator<Item = crate::occupants::Occupant>) -> Self {
        let mut labels: HashMap<u64, BTreeMap<String, String>> = HashMap::new();
        let mut keys = BTreeSet::new();
        for o in rows {
            keys.extend(o.labels.keys().cloned());
            labels.entry(o.occupant).or_insert(o.labels);
        }
        Self { labels, keys }
    }

    pub fn len(&self) -> usize {
        self.labels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.labels.is_empty()
    }

    fn with(&self, labels: &Labels) -> Option<Labels> {
        let occ: u64 = labels.inner.get(OCCUPANT_LABEL)?.parse().ok()?;
        let extra = self.labels.get(&occ)?;
        let mut out = labels.clone();
        for (k, v) in extra {
            out.inner.entry(k.clone()).or_insert_with(|| v.clone());
        }
        Some(out)
    }
}

impl ColumnRelabel for OccupantLabels {
    /// Returns true: an occupant's labels do not change, and each occupant
    /// column presents as one label set for all its samples. A segment
    /// naming an occupant the stream does not describe saves no state
    /// (see [`SegmentedSource::handover`](crate::segmented::SegmentedSource::handover)).
    fn identities_are_fixed(&self) -> bool {
        true
    }

    fn identities(&self, _name: &str, labels: &Labels) -> Option<Vec<Labels>> {
        self.with(labels).map(|l| vec![l])
    }

    fn split(&self, _name: &str, labels: &Labels, timestamps: &[u64]) -> Option<Vec<Run>> {
        self.with(labels).map(|l| vec![(l, 0..timestamps.len())])
    }

    fn at(&self, _name: &str, labels: &Labels, _timestamp: u64) -> Option<Labels> {
        self.with(labels)
    }

    fn segment_filter(&self, _name: &str, filter: &Labels) -> Labels {
        let mut f = filter.clone();
        f.inner.retain(|k, _| !self.keys.contains(k));
        f
    }
}

#[cfg(test)]
mod relabel_tests {
    use super::*;
    use crate::occupants::Occupant;

    fn occ(n: u64, pairs: &[(&str, &str)]) -> Occupant {
        Occupant {
            occupant: n,
            labels: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn the_relabel_adds_an_occupants_labels_and_keeps_its_number() {
        let r = OccupantLabels::new([occ(5, &[("comm", "w"), ("id", "3")])]);
        let col = Labels::from([(OCCUPANT_LABEL, "5"), ("state", "user")]);
        let got = r.identities("cpu", &col).unwrap();
        assert_eq!(
            got,
            vec![Labels::from([
                (OCCUPANT_LABEL, "5"),
                ("state", "user"),
                ("comm", "w"),
                ("id", "3")
            ])]
        );
        // An occupant the stream does not know is left alone.
        assert!(r
            .identities("cpu", &Labels::from([(OCCUPANT_LABEL, "6")]))
            .is_none());
        // Filters on occupant keys come off the segment filter.
        let f = r.segment_filter("cpu", &Labels::from([("comm", "w"), ("state", "user")]));
        assert_eq!(f, Labels::from([("state", "user")]));
    }
}
