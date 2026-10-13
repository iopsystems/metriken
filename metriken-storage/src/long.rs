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

/// One segment's file metadata, as [`SegmentMetadataMerge`] and
/// [`merge_segment_metadata`] read it.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
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

impl<'a> SegmentMetadata<'a> {
    /// One segment's file metadata: its key-value entries without
    /// `ARROW:schema`, its columns, and its parquet row count.
    pub fn new(
        key_value: &'a [parquet::file::metadata::KeyValue],
        fields: &'a arrow::datatypes::Fields,
        rows: u64,
    ) -> Self {
        SegmentMetadata {
            key_value,
            fields,
            rows,
        }
    }
}

/// The file key-value metadata of the segment that concatenating `inputs`
/// in order produces, or `None` when they cannot be merged.
///
/// [`SegmentMetadataMerge`] over the inputs: the same rule, for a caller
/// that has every input at once. See there for the rule.
pub fn merge_segment_metadata(
    inputs: &[SegmentMetadata<'_>],
) -> Option<Vec<parquet::file::metadata::KeyValue>> {
    let (first, rest) = inputs.split_first()?;
    let mut merge = SegmentMetadataMerge::start(first)?;
    rest.iter()
        .all(|next| merge.extend(next))
        .then(|| merge.finish())
}

/// The file key-value metadata of a run of segments being concatenated, built
/// one segment at a time: [`start`](Self::start) with the first,
/// [`extend`](Self::extend) with each next one, [`finish`](Self::finish) for
/// the merged segment's metadata. For a compaction's metadata merge: a long
/// segment's occupant lists change whenever the population does, and merging
/// only segments with identical metadata leaves a long stream nearly as it
/// was sealed.
///
/// Each segment's lists are decoded once, when it is added, and the run keeps
/// them decoded, so a run costs time linear in its segments' metadata.
///
/// Long segments merge exactly by union. The merged [`OCCUPANTS_KEY`] is the
/// union of the inputs' lists. A column's list ([`column_occupants_key`]) is
/// the union over the inputs that have the column, each contributing its own
/// list for the column or, without one, its segment list; it is omitted
/// when that union is the merged segment list, as the writer omits it.
/// Every other key, [`LAYOUT_KEY`] and the format version among them, must
/// have the same value in every input. The merged metadata has the first
/// input's keys in its order, with the merged segment list in place of its
/// own and the column lists after them.
///
/// A run whose first segment is not long takes only segments with the same
/// entries as it, in any order, and is written with the first's order; a
/// long run takes no segment that is not long. A long segment with no
/// segment list, a list that does not decode within its segment's rows, or
/// a key given twice is refused; this never panics on a malformed list.
#[derive(Debug, Clone)]
pub struct SegmentMetadataMerge(Merge);

#[derive(Debug, Clone)]
enum Merge {
    /// The first segment is not long: its metadata in file order, and
    /// sorted, the entries every other segment must repeat in some order.
    Identical {
        first: Vec<parquet::file::metadata::KeyValue>,
        sorted: Vec<parquet::file::metadata::KeyValue>,
    },
    Long(LongMerge),
}

#[derive(Debug, Clone)]
struct LongMerge {
    /// The first segment's metadata, for the order of the merged keys.
    first: Vec<parquet::file::metadata::KeyValue>,
    /// The keys other than the occupant lists, which every segment must
    /// have with the same values.
    rest: BTreeMap<String, Option<String>>,
    /// The union of the segment lists.
    occupants: BTreeSet<u64>,
    /// Each column, in the order it first appears, with the union of the
    /// occupants it has values for.
    columns: Vec<(String, BTreeSet<u64>)>,
    /// Where each column is in `columns`.
    column_index: HashMap<String, usize>,
}

/// One long segment's metadata, decoded: its segment list, its column lists,
/// and its other keys.
struct LongSegment<'a> {
    occupants: Vec<u64>,
    columns: HashMap<&'a str, Vec<u64>>,
    rest: BTreeMap<String, Option<String>>,
}

/// `kv` sorted by key, then value, so two lists of the same entries compare
/// equal whatever their order.
fn sorted(kv: &[parquet::file::metadata::KeyValue]) -> Vec<parquet::file::metadata::KeyValue> {
    let mut kv = kv.to_vec();
    kv.sort_by(|a, b| (&a.key, &a.value).cmp(&(&b.key, &b.value)));
    kv
}

fn is_long(kv: &[parquet::file::metadata::KeyValue]) -> bool {
    kv.iter()
        .any(|e| e.key == LAYOUT_KEY && e.value.as_deref() == Some(LAYOUT_LONG))
}

/// Decode a long segment's metadata, or `None` for a segment that a long run
/// cannot take: no segment list, a list that does not decode within the
/// segment's rows, or a key given twice.
fn decode_long<'a>(input: &SegmentMetadata<'a>) -> Option<LongSegment<'a>> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut occupants = None;
    let mut columns = HashMap::new();
    let mut rest = BTreeMap::new();
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
            rest.insert(e.key.clone(), e.value.clone());
        }
    }
    Some(LongSegment {
        occupants: occupants?,
        columns,
        rest,
    })
}

impl LongMerge {
    /// Add a decoded segment: its segment list to the union, and to each
    /// column it has, its list for the column or its segment list.
    fn add(&mut self, fields: &arrow::datatypes::Fields, segment: LongSegment<'_>) {
        for f in fields.iter() {
            let i = match self.column_index.get(f.name()) {
                Some(i) => *i,
                None => {
                    self.column_index
                        .insert(f.name().clone(), self.columns.len());
                    self.columns.push((f.name().clone(), BTreeSet::new()));
                    self.columns.len() - 1
                }
            };
            let list = segment
                .columns
                .get(f.name().as_str())
                .unwrap_or(&segment.occupants);
            self.columns[i].1.extend(list);
        }
        self.occupants.extend(segment.occupants);
    }
}

impl SegmentMetadataMerge {
    /// Begin a run at `first`, or `None` for a long segment that cannot be
    /// merged (see the type's documentation).
    pub fn start(first: &SegmentMetadata<'_>) -> Option<Self> {
        if !is_long(first.key_value) {
            return Some(Self(Merge::Identical {
                first: first.key_value.to_vec(),
                sorted: sorted(first.key_value),
            }));
        }
        let segment = decode_long(first)?;
        let mut long = LongMerge {
            first: first.key_value.to_vec(),
            rest: segment.rest.clone(),
            occupants: BTreeSet::new(),
            columns: Vec::new(),
            column_index: HashMap::new(),
        };
        long.add(first.fields, segment);
        Some(Self(Merge::Long(long)))
    }

    /// Add `next` to the run and return `true`, or return `false` and leave
    /// the run as it was when `next` cannot be merged into it.
    pub fn extend(&mut self, next: &SegmentMetadata<'_>) -> bool {
        match &mut self.0 {
            Merge::Identical { sorted: s, .. } => {
                next.key_value.len() == s.len() && sorted(next.key_value) == *s
            }
            Merge::Long(long) => {
                if !is_long(next.key_value) {
                    return false;
                }
                match decode_long(next) {
                    Some(segment) if segment.rest == long.rest => {
                        long.add(next.fields, segment);
                        true
                    }
                    _ => false,
                }
            }
        }
    }

    /// The merged segment's file key-value metadata.
    pub fn finish(self) -> Vec<parquet::file::metadata::KeyValue> {
        use parquet::file::metadata::KeyValue;

        let long = match self.0 {
            Merge::Identical { first, .. } => return first,
            Merge::Long(long) => long,
        };
        let all = encode_occupant_ranges(long.occupants);
        let column_keys = long.columns.into_iter().filter_map(|(column, has)| {
            let encoded = encode_occupant_ranges(has);
            (encoded != all).then(|| KeyValue::new(column_occupants_key(&column), encoded))
        });
        let mut out: Vec<KeyValue> = long
            .first
            .into_iter()
            .filter(|e| !e.key.starts_with(COLUMN_OCCUPANTS_PREFIX))
            .map(|e| {
                if e.key == OCCUPANTS_KEY {
                    KeyValue::new(OCCUPANTS_KEY.to_string(), all.clone())
                } else {
                    e
                }
            })
            .collect();
        out.extend(column_keys);
        out
    }
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
            let more = kv(&[("metriken.format", "1"), ("k", "v"), ("k2", "v")]);
            assert_eq!(merge(&[(&one, &f, 9), (&more, &f, 9)]), None);
            assert_eq!(merge(&[(&more, &f, 9), (&one, &f, 9)]), None);
        }

        /// Order is not part of the comparison: the same entries in another
        /// order merge, and the merged segment has the first's order.
        #[test]
        fn segments_not_long_merge_in_any_order() {
            let f = fields(&["timestamp", "a"]);
            let one = kv(&[("metriken.format", "1"), ("k", "v")]);
            let reordered = kv(&[("k", "v"), ("metriken.format", "1")]);
            assert_eq!(
                merge(&[(&one, &f, 9), (&reordered, &f, 9)]).unwrap(),
                pairs(&one)
            );
            assert_eq!(
                merge(&[(&reordered, &f, 9), (&one, &f, 9)]).unwrap(),
                pairs(&reordered)
            );
        }

        /// A refused segment leaves the run as it was: finishing it gives
        /// what the segments before the refusal give, and the run can still
        /// take a segment that merges.
        #[test]
        fn a_refused_segment_leaves_the_run_unchanged() {
            let f = fields(&["timestamp", "occupant", "a", "b"]);
            let one = long("0-2", &[("b", "0")]);
            let two = long("1-3", &[("a", "1-2")]);
            for bad in [
                long("5-6", &[("a", "x")]),
                kv(&[("metriken.format", "1")]),
                [long("5-6", &[]), kv(&[("x", "y")])].concat(),
            ] {
                let segment = |kv| SegmentMetadata {
                    key_value: kv,
                    fields: &f,
                    rows: 9,
                };
                let mut run = SegmentMetadataMerge::start(&segment(&one)).unwrap();
                assert!(!run.extend(&segment(&bad)), "{bad:?}");
                assert_eq!(pairs(&run.clone().finish()), pairs(&one), "{bad:?}");
                assert!(run.extend(&segment(&two)));
                assert_eq!(
                    Some(pairs(&run.finish())),
                    merge(&[(&one, &f, 9), (&two, &f, 9)]),
                    "{bad:?}"
                );
            }
        }

        /// A column first seen in a later segment contributes from the
        /// segments that have it only, and its list follows those of the
        /// columns seen earlier.
        #[test]
        fn a_column_first_seen_later_contributes_from_there() {
            let ab = fields(&["timestamp", "occupant", "a", "b"]);
            let abc = fields(&["timestamp", "occupant", "a", "b", "c"]);
            let one = long("0-1", &[("b", "0")]);
            let two = long("2-3", &[]);
            let three = long("4", &[("b", "4"), ("c", "4")]);
            assert_eq!(
                merge(&[(&one, &ab, 9), (&two, &abc, 9), (&three, &abc, 9)]).unwrap(),
                pairs(&long("0-4", &[("b", "0,2-4"), ("c", "2-4")]))
            );
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
