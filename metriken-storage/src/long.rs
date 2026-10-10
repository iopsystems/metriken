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

/// File key-value metadata key naming the segment layout.
pub const LAYOUT_KEY: &str = "metriken.layout";

/// [`LAYOUT_KEY`]'s value for a long segment.
pub const LAYOUT_LONG: &str = "long";

/// File key-value metadata key listing a long segment's occupants.
pub const OCCUPANTS_KEY: &str = "metriken.occupants";

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
    /// (see `metriken_query::SegmentedParquetReader::handover`).
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
