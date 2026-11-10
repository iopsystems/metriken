use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

use regex::Regex;

/// Keys in a column's field metadata that describe the column rather than
/// the series, and so never become labels.
///
/// `metric` is the series name (exposed to PromQL as `__name__`),
/// `metric_type` and `unit` describe the value, `grouping_power` /
/// `max_value_power` are a histogram's bucket configuration,
/// [`OVERFLOW_KEY`] / [`OVERFLOW_BOUND_KEY`] name a histogram's overflow
/// series and its highest finite bound, and [`SCALE_KEY`] is the factor a
/// histogram's values were stored multiplied by. Every loader
/// consults this one list: the parquet path and the `ingest` path used to
/// keep separate lists that had drifted by the two histogram keys, so the
/// same recording carried two extra labels per histogram when viewed live.
///
/// `storage_keys_are_pinned` holds the list still, and each loader has a
/// test that a histogram column carrying every key here yields no labels
/// from them (`ingest_does_not_turn_histogram_configuration_into_labels`,
/// `parquet_does_not_turn_histogram_configuration_into_labels`), so a
/// pre-filter added to one loader cannot make them disagree unnoticed.
/// A key added here is still a label in an archive's stored names indexes
/// (their columns frames) written before it, so it needs a new index
/// version as well (`names.rs`), unless the version it joins has not been
/// released yet.
///
/// An exporter label whose key is in this list is not a label either.
/// metriken-recorder 0.13.0 and later store one as `exported_<key>`;
/// before, two series that differed only by one were stored as two columns
/// with one label set, which a reader reports through
/// [`DataSource::duplicated_series`](crate::DataSource::duplicated_series).
///
/// A slice rather than an array so that adding a key is not a change to a
/// public type.
pub const STORAGE_KEYS: &[&str] = &[
    "metric",
    "metric_type",
    "unit",
    "grouping_power",
    "max_value_power",
    OVERFLOW_KEY,
    OVERFLOW_BOUND_KEY,
    SCALE_KEY,
];

/// The histogram column key holding the factor its values were stored
/// multiplied by, as an `f64`: a stored bucket bound is the observation, in
/// the column's `unit` (or the exporter's unit when there is none), times
/// it. Written by metriken-recorder's Prometheus converter when the factor
/// is not 1, which is when the bounds would otherwise have been truncated
/// to integers (a smallest bound below 1). A reader that wants the values
/// in their unit divides by it; metriken-query does not.
pub const SCALE_KEY: &str = "scale";

/// The histogram column key naming its overflow series: the metric name of
/// a counter, with the histogram's labels, that counts the observations
/// above the histogram's highest finite bound. A Prometheus histogram's
/// `+Inf` bucket minus its highest finite bucket. Written by
/// metriken-recorder's Prometheus converter; a histogram without it has no
/// overflow series.
pub const OVERFLOW_KEY: &str = "overflow";

/// The histogram column key holding its highest finite bound, as an `f64`
/// in the histogram's value units: every observation its
/// [overflow series](OVERFLOW_KEY) counts is above it.
pub const OVERFLOW_BOUND_KEY: &str = "overflow_bound";

/// Whether `key` is a storage key — see [`STORAGE_KEYS`].
pub fn is_storage_key(key: &str) -> bool {
    STORAGE_KEYS.contains(&key)
}

/// Whether `name` is an internal label: one the engine needs for series
/// identity that a person reading a chart does not.
///
/// The rule is Prometheus's: "label names beginning with `__` MUST be
/// reserved for internal Prometheus use". An internal label is part of a
/// series' identity and matchable in a selector — `foo{__run__="1"}` works —
/// and it is dropped by `without` and by default binary-op matching alongside
/// `__name__`, except `__recording__` ([`is_matched_label`]).
///
/// This crate emits internal labels on every result it returns. Hiding them
/// from listings and legends is the consumer's contract, not something done
/// here, which is why the predicate is public.
///
/// The one `__` key a writer in this workspace emits into column metadata
/// is [`PROMETHEUS_NAME_LABEL`](crate::PROMETHEUS_NAME_LABEL), the name an
/// exporter served a renamed series under (metriken-recorder's Prometheus
/// converter). The loader does not check for others: a key with the
/// prefix that did arrive would be kept as a label (the loaders strip only
/// storage keys) and could collide with a label the engine injects.
///
/// The engine's own internal labels are `__name__` (the series name) and
/// `__run__` (a histogram whose bucket configuration changed mid-recording).
/// Readers above this crate may add their own under the same rule.
pub fn is_internal_label(name: &str) -> bool {
    name.starts_with("__")
}

/// Whether binary-operator matching and `without` grouping consider `name`:
/// every label that is not internal, and
/// [`RECORDING_LABEL`](crate::labeled::RECORDING_LABEL).
///
/// `__recording__` is internal so that a listing or a legend leaves it out,
/// but it is the only label telling apart two recordings whose label sets
/// are identical. Matching without it pairs one recording's `g` with the
/// other's `h`, and `sum without (cpu)` merges the two recordings.
pub fn is_matched_label(name: &str) -> bool {
    !is_internal_label(name) || name == crate::labeled::RECORDING_LABEL
}

#[derive(Default, Eq, PartialEq, Hash, Clone, Debug)]
pub struct Labels {
    pub inner: BTreeMap<String, String>,
}

impl Labels {
    /// The label set a column's field metadata carries: everything that is
    /// not a [storage key](STORAGE_KEYS).
    ///
    /// The one place metadata becomes labels, for both loaders, so they
    /// cannot disagree about which keys are labels.
    pub fn from_metadata<'a, K, V>(metadata: impl IntoIterator<Item = (&'a K, &'a V)>) -> Self
    where
        K: AsRef<str> + 'a,
        V: AsRef<str> + 'a,
    {
        let inner = metadata
            .into_iter()
            .filter(|(k, _)| !is_storage_key(k.as_ref()))
            .map(|(k, v)| (k.as_ref().to_string(), v.as_ref().to_string()))
            .collect();
        Labels { inner }
    }
}

/// How a matcher of a filter compares a label value.
///
/// A filter ([`Labels::matches`]) holds one matcher per label, encoded in the
/// value: [`MatchOp::encode`] writes it and [`Labels::matches`] reads it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MatchOp {
    /// `=`: the value equals the operand.
    Equal,
    /// `!=`: the value does not equal the operand, or the label is absent.
    NotEqual,
    /// `=~`: the value matches the operand, a regular expression anchored at
    /// both ends.
    Regex,
    /// `!~`: the value does not match the operand, or the label is absent.
    NotRegex,
}

impl MatchOp {
    /// The filter value for this matcher with `operand`.
    ///
    /// `=` is the operand itself, or the operand after `=` when it starts
    /// with `!`, `~` or `=`. `=~` is `~` and the pattern, `!~` is `!` and the
    /// pattern, and `!=` is `!` and the operand escaped as a regex.
    pub fn encode(self, operand: &str) -> String {
        match self {
            MatchOp::Equal if operand.starts_with(['!', '~', '=']) => format!("={operand}"),
            MatchOp::Equal => operand.to_string(),
            MatchOp::NotEqual => format!("!{}", regex::escape(operand)),
            MatchOp::Regex => format!("~{operand}"),
            MatchOp::NotRegex => format!("!{operand}"),
        }
    }
}

/// The most patterns [`PATTERNS`] holds; reaching it clears them.
const PATTERN_CACHE: usize = 256;

/// Whether `c` is special in a regex outside a character class, without the
/// `x` flag (which needs `(` to set).
fn is_meta(c: char) -> bool {
    matches!(
        c,
        '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$'
    )
}

thread_local! {
    /// Patterns compiled on this thread, so a filter's pattern is compiled
    /// once and not once per series; `None` for one that does not compile.
    static PATTERNS: RefCell<HashMap<String, Option<Regex>>> = RefCell::new(HashMap::new());
}

/// Whether `value` matches `pattern`, anchored at both ends. A pattern that
/// does not compile matches nothing.
fn match_pattern(value: &str, pattern: &str) -> bool {
    // A list of literals, the common case, is decided without a regex.
    let literal = |alternative: &str| !alternative.contains(is_meta);
    if pattern.split('|').all(literal) {
        return pattern.split('|').any(|alternative| alternative == value);
    }
    PATTERNS.with(|patterns| {
        let mut patterns = patterns.borrow_mut();
        if !patterns.contains_key(pattern) {
            if patterns.len() >= PATTERN_CACHE {
                patterns.clear();
            }
            let compiled = Regex::new(&format!("^(?:{pattern})$")).ok();
            patterns.insert(pattern.to_string(), compiled);
        }
        patterns[pattern]
            .as_ref()
            .is_some_and(|regex| regex.is_match(value))
    })
}

impl From<&[(&str, &str)]> for Labels {
    fn from(other: &[(&str, &str)]) -> Self {
        Labels {
            inner: other
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }
}

impl From<BTreeMap<String, String>> for Labels {
    fn from(inner: BTreeMap<String, String>) -> Self {
        Labels { inner }
    }
}

impl From<()> for Labels {
    fn from(_other: ()) -> Self {
        Labels::default()
    }
}

impl<const N: usize> From<[(&str, &str); N]> for Labels {
    fn from(other: [(&str, &str); N]) -> Self {
        Labels {
            inner: other
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }
}

impl<const N: usize> From<[(String, String); N]> for Labels {
    fn from(other: [(String, String); N]) -> Self {
        Labels {
            inner: other.iter().cloned().collect(),
        }
    }
}

impl<const N: usize> From<[(&str, String); N]> for Labels {
    fn from(other: [(&str, String); N]) -> Self {
        Labels {
            inner: other
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        }
    }
}

impl From<&mut dyn Iterator<Item = (&str, &str)>> for Labels {
    fn from(other: &mut dyn Iterator<Item = (&str, &str)>) -> Self {
        Self {
            inner: other.map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }
}

impl Labels {
    /// Whether this label set satisfies every matcher of the filter `other`,
    /// one per label, encoded as [`MatchOp::encode`] writes them. A label
    /// this set does not carry fails `=` and `=~` and passes `!=` and `!~`.
    pub fn matches(&self, other: &Labels) -> bool {
        for (label, value) in other.inner.iter() {
            if let Some(pattern) = value.strip_prefix('!') {
                if let Some(v) = self.inner.get(label) {
                    if match_pattern(v, pattern) {
                        return false;
                    }
                }
            } else if let Some(pattern) = value.strip_prefix('~') {
                let Some(v) = self.inner.get(label) else {
                    return false;
                };
                if !match_pattern(v, pattern) {
                    return false;
                }
            } else if let Some(v) = self.inner.get(label) {
                let operand = value.strip_prefix('=').unwrap_or(value);
                if v != operand {
                    return false;
                }
            } else {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(op: MatchOp, operand: &str) -> Labels {
        Labels::from([("k", op.encode(operand))])
    }

    fn value(v: &str) -> Labels {
        Labels::from([("k", v)])
    }

    /// Each operator decides as Prometheus's does for a series carrying the
    /// label, for operands holding the characters the encoding uses.
    #[test]
    fn an_encoded_matcher_decides_as_its_operator() {
        let values = ["", "a", "a|b", "!a", "~a", "=a", "!=a", "a.b", "axb", "(a)"];
        for operand in values {
            for v in values {
                assert_eq!(
                    value(v).matches(&filter(MatchOp::Equal, operand)),
                    v == operand,
                    "{v} = {operand}"
                );
                assert_eq!(
                    value(v).matches(&filter(MatchOp::NotEqual, operand)),
                    v != operand,
                    "{v} != {operand}"
                );
            }
        }
        let regex = |p: &str, v: &str| {
            regex::Regex::new(&format!("^(?:{p})$"))
                .unwrap()
                .is_match(v)
        };
        for pattern in ["a", "a|b", "a.b", "a\\.b", "[!~=]a", "", ".*", "\\(a\\)"] {
            for v in values {
                assert_eq!(
                    value(v).matches(&filter(MatchOp::Regex, pattern)),
                    regex(pattern, v),
                    "{v} =~ {pattern}"
                );
                assert_eq!(
                    value(v).matches(&filter(MatchOp::NotRegex, pattern)),
                    !regex(pattern, v),
                    "{v} !~ {pattern}"
                );
            }
        }
    }

    /// A series without the label: only the negative operators match it.
    #[test]
    fn an_absent_label_matches_only_a_negative_matcher() {
        let none = Labels::from([("other", "a")]);
        assert!(!none.matches(&filter(MatchOp::Equal, "a")));
        assert!(!none.matches(&filter(MatchOp::Regex, ".*")));
        assert!(none.matches(&filter(MatchOp::NotEqual, "a")));
        assert!(none.matches(&filter(MatchOp::NotRegex, "a")));
    }

    /// A pattern that does not compile matches nothing, so `!~` with it
    /// matches everything; more patterns than the cache holds still match.
    #[test]
    fn a_bad_pattern_matches_nothing_and_the_cache_is_bounded() {
        assert!(!value("(").matches(&filter(MatchOp::Regex, "(")));
        assert!(value("(").matches(&filter(MatchOp::NotRegex, "(")));
        for i in 0..PATTERN_CACHE * 2 + 1 {
            let pattern = format!("v{i}w?");
            assert!(value(&format!("v{i}")).matches(&filter(MatchOp::Regex, &pattern)));
            assert!(!value("v").matches(&filter(MatchOp::Regex, &pattern)));
        }
        assert!(PATTERNS.with(|p| p.borrow().len()) <= PATTERN_CACHE);
    }

    /// The list is the contract both loaders read. A key added to one loader
    /// and not the other is how the histogram configuration keys became
    /// labels on the live path and not the recorded one; pinning the list
    /// here means such a change is a deliberate edit to one place.
    #[test]
    fn storage_keys_are_pinned() {
        assert_eq!(
            STORAGE_KEYS,
            &[
                "metric",
                "metric_type",
                "unit",
                "grouping_power",
                "max_value_power",
                "overflow",
                "overflow_bound",
                "scale"
            ]
        );
        for k in STORAGE_KEYS {
            assert!(is_storage_key(k));
            assert!(
                !is_internal_label(k),
                "{k}: a storage key is not a label at all, internal or otherwise"
            );
        }
        assert!(!is_storage_key("cpu"));
        assert!(!is_storage_key("__name__"));
    }

    /// Prometheus's rule, exactly: the prefix and nothing else decides.
    #[test]
    fn the_double_underscore_prefix_is_the_whole_rule() {
        for name in ["__name__", "__run__", "__incarnation__", "__x", "___"] {
            assert!(is_internal_label(name), "{name}");
        }
        for name in ["name", "_name", "x__", "cpu", "", "_"] {
            assert!(!is_internal_label(name), "{name}");
        }
    }

    /// Metadata becomes labels by removing exactly the storage keys: a
    /// histogram's configuration keys are not labels, and an internal label
    /// that arrives in metadata is kept — it is identity, and hiding it is
    /// the consumer's job, not the loader's.
    #[test]
    fn from_metadata_removes_the_storage_keys_and_nothing_else() {
        let meta: BTreeMap<String, String> = [
            ("metric", "latency"),
            ("metric_type", "histogram"),
            ("unit", "nanoseconds"),
            ("grouping_power", "7"),
            ("max_value_power", "64"),
            ("scale", "10"),
            ("cpu", "3"),
            ("__run__", "1"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let labels = Labels::from_metadata(meta.iter());
        assert_eq!(
            labels,
            Labels::from([("cpu", "3"), ("__run__", "1")]),
            "{labels:?}"
        );
    }
}
