use std::collections::BTreeMap;

/// Keys in a column's field metadata that describe the column rather than
/// the series, and so never become labels.
///
/// `metric` is the series name (exposed to PromQL as `__name__`),
/// `metric_type` and `unit` describe the value, and `grouping_power` /
/// `max_value_power` are a histogram's bucket configuration. Every loader
/// consults this one list: the parquet path and the `ingest` path used to
/// keep separate lists that had drifted by the two histogram keys, so the
/// same recording carried two extra labels per histogram when viewed live.
///
/// `storage_keys_are_pinned` holds the list still, and each loader has a
/// test that a histogram column carrying every key here yields no labels
/// from them (`ingest_does_not_turn_histogram_configuration_into_labels`,
/// `parquet_does_not_turn_histogram_configuration_into_labels`), so a
/// pre-filter added to one loader cannot make them disagree unnoticed.
///
/// A slice rather than an array so that adding a key is not a change to a
/// public type.
pub const STORAGE_KEYS: &[&str] = &[
    "metric",
    "metric_type",
    "unit",
    "grouping_power",
    "max_value_power",
];

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
/// `__name__`.
///
/// This crate emits internal labels on every result it returns. Hiding them
/// from listings and legends is the consumer's contract, not something done
/// here, which is why the predicate is public.
///
/// No writer in this workspace emits a `__` key into column metadata, and
/// the loader does not check for one: a key with the prefix that did arrive
/// would be kept as a label (the loaders strip only storage keys) and could
/// collide with a label the engine injects.
///
/// The engine's own internal labels are `__name__` (the series name) and
/// `__run__` (a histogram whose bucket configuration changed mid-recording).
/// Readers above this crate may add their own under the same rule.
pub fn is_internal_label(name: &str) -> bool {
    name.starts_with("__")
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
    pub(crate) fn from_metadata<'a, K, V>(
        metadata: impl IntoIterator<Item = (&'a K, &'a V)>,
    ) -> Self
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

fn match_pattern(value: &str, pattern: &str) -> bool {
    if pattern.contains('|') {
        let inner = if pattern.starts_with('(') && pattern.ends_with(')') {
            &pattern[1..pattern.len() - 1]
        } else {
            pattern
        };
        inner.split('|').any(|option| {
            if option.contains("\\.") {
                value == option.replace("\\.", ".")
            } else {
                value == option
            }
        })
    } else if pattern.contains("\\.") {
        value == pattern.replace("\\.", ".")
    } else {
        value == pattern
    }
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
                if !match_pattern(v, value) {
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
                "max_value_power"
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
