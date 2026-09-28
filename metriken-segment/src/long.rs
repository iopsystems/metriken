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
