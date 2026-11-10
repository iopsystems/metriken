//! Units in Prometheus metric names, and the names they are stored under.
//!
//! metriken-recorder's Prometheus converter stores a series whose name ends
//! in a unit token in the normalized unit (nanoseconds or bytes) under a
//! name whose token is the normalized one, and keeps the exporter's name in
//! the internal label [`PROMETHEUS_NAME_LABEL`]. metriken-query uses
//! [`fallback_candidates`] to find such a series when a query names it by
//! the exporter's name. Both go through [`normalize`], so the two cannot
//! disagree about which names a rename produces.

/// The internal label holding the name an exporter served a series under,
/// on a series stored under another name ([`normalize`]). Part of the
/// series' identity, matchable in a selector, and hidden from listings and
/// legends like every internal label ([`is_internal_label`](crate::is_internal_label)).
pub const PROMETHEUS_NAME_LABEL: &str = "__prometheus_name__";

/// The suffixes removed before a name's unit token is looked for, and kept
/// on the stored name.
pub const SUFFIXES: &[&str] = &["_bucket", "_sum", "_count", "_total"];

/// What a unit token is stored as.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Unit {
    /// The token the stored name ends in (before its suffix).
    pub token: &'static str,
    /// The `unit` storage key's value.
    pub unit: &'static str,
    /// What a value in the token's unit is multiplied by to be in `unit`.
    pub factor: f64,
}

const fn unit(token: &'static str, unit: &'static str, factor: f64) -> Unit {
    Unit {
        token,
        unit,
        factor,
    }
}

const NANOSECONDS: &str = "nanoseconds";
const BYTES: &str = "bytes";

/// Every token a name or an OpenMetrics `# UNIT` line is matched against,
/// and what it is stored as.
pub const TOKENS: &[(&str, Unit)] = &[
    ("seconds", unit(NANOSECONDS, NANOSECONDS, 1e9)),
    ("milliseconds", unit(NANOSECONDS, NANOSECONDS, 1e6)),
    ("ms", unit(NANOSECONDS, NANOSECONDS, 1e6)),
    ("msec", unit(NANOSECONDS, NANOSECONDS, 1e6)),
    ("microseconds", unit(NANOSECONDS, NANOSECONDS, 1e3)),
    ("us", unit(NANOSECONDS, NANOSECONDS, 1e3)),
    ("nanoseconds", unit(NANOSECONDS, NANOSECONDS, 1.0)),
    ("ns", unit(NANOSECONDS, NANOSECONDS, 1.0)),
    ("bytes", unit(BYTES, BYTES, 1.0)),
    ("kilobytes", unit(BYTES, BYTES, 1e3)),
    ("kb", unit(BYTES, BYTES, 1e3)),
    ("kibibytes", unit(BYTES, BYTES, 1024.0)),
    ("kib", unit(BYTES, BYTES, 1024.0)),
    ("megabytes", unit(BYTES, BYTES, 1e6)),
    ("mb", unit(BYTES, BYTES, 1e6)),
    ("mebibytes", unit(BYTES, BYTES, 1_048_576.0)),
    ("mib", unit(BYTES, BYTES, 1_048_576.0)),
    ("gigabytes", unit(BYTES, BYTES, 1e9)),
    ("gb", unit(BYTES, BYTES, 1e9)),
    ("gibibytes", unit(BYTES, BYTES, 1_073_741_824.0)),
    ("gib", unit(BYTES, BYTES, 1_073_741_824.0)),
    ("bits", unit(BYTES, BYTES, 0.125)),
    ("percent", unit("percent", "percentage", 1.0)),
    ("ratio", unit("ratio", "ratio", 1.0)),
    ("celsius", unit("celsius", "celsius", 1.0)),
    ("hertz", unit("hertz", "hertz", 1.0)),
    ("cores", unit("cores", "cores", 1.0)),
];

/// What `token` is stored as, or `None` when it is not a unit token.
pub fn unit_of(token: &str) -> Option<Unit> {
    TOKENS
        .iter()
        .find(|(t, _)| *t == token)
        .map(|(_, unit)| *unit)
}

/// `name` split into its stem and one of [`SUFFIXES`] (empty when it has
/// none, or when the suffix is the whole name).
pub fn split_suffix(name: &str) -> (&str, &str) {
    for suffix in SUFFIXES {
        if let Some(stem) = name.strip_suffix(suffix) {
            if !stem.is_empty() {
                return (stem, &name[stem.len()..]);
            }
        }
    }
    (name, "")
}

/// A series' stored name, unit and factor.
#[derive(Clone, Debug, PartialEq)]
pub struct Normalized {
    /// The name the series is stored under: the exporter's name, or that
    /// name with its unit token replaced (or, from a `# UNIT` line, added).
    pub name: String,
    /// The `unit` storage key, or `None` for a `_count` or `_bucket`
    /// sample, which is a count whatever its metric's unit.
    pub unit: Option<&'static str>,
    /// What each value is multiplied by: 1 for a count.
    pub factor: f64,
}

/// The stored name, unit and factor of a series an exporter serves as
/// `name`, with `unit_line` the value of its metric's OpenMetrics `# UNIT`
/// line, if any. `None` when neither gives a unit in [`TOKENS`].
///
/// The token is the last `_`-separated word of `name` after removing one of
/// [`SUFFIXES`]; a name with no `_` there has none. A `unit_line` in
/// [`TOKENS`] decides the unit over the name's token. The stored name is
/// `name` with its token replaced by the unit's stored token; with no token
/// in the name, the stored token is added before the suffix when the factor
/// is not 1, so a series whose values are scaled is always renamed, and the
/// name is kept when it is 1. A `_count` or `_bucket` sample is renamed the
/// same way and not scaled.
pub fn normalize(name: &str, unit_line: Option<&str>) -> Option<Normalized> {
    let (stem, suffix) = split_suffix(name);
    let token = stem
        .rsplit_once('_')
        .filter(|(head, _)| !head.is_empty())
        .and_then(|(head, token)| unit_of(token).map(|unit| (head, unit)));
    let from_line = unit_line.and_then(unit_of);
    let unit = from_line.or(token.map(|(_, unit)| unit))?;
    let stem = match token {
        Some((head, _)) => format!("{head}_{}", unit.token),
        None if unit.factor != 1.0 => format!("{stem}_{}", unit.token),
        None => stem.to_string(),
    };
    let count = matches!(suffix, "_count" | "_bucket");
    Some(Normalized {
        name: format!("{stem}{suffix}"),
        unit: (!count).then_some(unit.unit),
        factor: if count { 1.0 } else { unit.factor },
    })
}

/// The names other than `name` that a series the exporter served as
/// `name` can be stored under, under any `# UNIT` line: where a query that
/// names `name` and finds nothing looks for a series whose
/// [`PROMETHEUS_NAME_LABEL`] is `name`. At most a handful, in a fixed
/// order.
pub fn fallback_candidates(name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let lines = std::iter::once(None).chain(TOKENS.iter().map(|(t, _)| Some(*t)));
    for line in lines {
        if let Some(n) = normalize(name, line) {
            if n.name != name && !out.contains(&n.name) {
                out.push(n.name);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(name: &str, line: Option<&str>) -> Option<(String, Option<&'static str>, f64)> {
        normalize(name, line).map(|n| (n.name, n.unit, n.factor))
    }

    #[test]
    fn each_token_class_normalizes() {
        let ns = Some("nanoseconds");
        let b = Some("bytes");
        for (name, want) in [
            ("lat_seconds", ("lat_nanoseconds", ns, 1e9)),
            ("lat_milliseconds", ("lat_nanoseconds", ns, 1e6)),
            ("lat_ms", ("lat_nanoseconds", ns, 1e6)),
            ("lat_msec", ("lat_nanoseconds", ns, 1e6)),
            ("lat_microseconds", ("lat_nanoseconds", ns, 1e3)),
            ("lat_us", ("lat_nanoseconds", ns, 1e3)),
            ("lat_nanoseconds", ("lat_nanoseconds", ns, 1.0)),
            ("lat_ns", ("lat_nanoseconds", ns, 1.0)),
            ("mem_bytes", ("mem_bytes", b, 1.0)),
            ("mem_kilobytes", ("mem_bytes", b, 1e3)),
            ("mem_kb", ("mem_bytes", b, 1e3)),
            ("mem_kibibytes", ("mem_bytes", b, 1024.0)),
            ("mem_kib", ("mem_bytes", b, 1024.0)),
            ("mem_megabytes", ("mem_bytes", b, 1e6)),
            ("mem_mb", ("mem_bytes", b, 1e6)),
            ("mem_mebibytes", ("mem_bytes", b, 1_048_576.0)),
            ("mem_mib", ("mem_bytes", b, 1_048_576.0)),
            ("mem_gigabytes", ("mem_bytes", b, 1e9)),
            ("mem_gb", ("mem_bytes", b, 1e9)),
            ("mem_gibibytes", ("mem_bytes", b, 1_073_741_824.0)),
            ("mem_gib", ("mem_bytes", b, 1_073_741_824.0)),
            ("net_bits", ("net_bytes", b, 0.125)),
            ("cpu_percent", ("cpu_percent", Some("percentage"), 1.0)),
            ("hit_ratio", ("hit_ratio", Some("ratio"), 1.0)),
            ("temp_celsius", ("temp_celsius", Some("celsius"), 1.0)),
            ("clock_hertz", ("clock_hertz", Some("hertz"), 1.0)),
            ("limit_cores", ("limit_cores", Some("cores"), 1.0)),
        ] {
            assert_eq!(
                stored(name, None),
                Some((want.0.to_string(), want.1, want.2)),
                "{name}"
            );
        }
    }

    #[test]
    fn a_suffix_is_kept_and_a_count_is_not_scaled() {
        let ns = Some("nanoseconds");
        assert_eq!(
            stored("rpc_ms_sum", None),
            Some(("rpc_nanoseconds_sum".into(), ns, 1e6))
        );
        assert_eq!(
            stored("rpc_ms_count", None),
            Some(("rpc_nanoseconds_count".into(), None, 1.0))
        );
        assert_eq!(
            stored("rpc_ms_bucket", None),
            Some(("rpc_nanoseconds_bucket".into(), None, 1.0))
        );
        assert_eq!(
            stored("cpu_seconds_total", None),
            Some(("cpu_nanoseconds_total".into(), ns, 1e9))
        );
        assert_eq!(
            stored("rx_bits_total", None),
            Some(("rx_bytes_total".into(), Some("bytes"), 0.125))
        );
    }

    #[test]
    fn a_name_without_a_unit_token_is_not_normalized() {
        for name in [
            "requests_total",
            "items",
            "seconds",
            "_seconds",
            "x_total",
            "total",
        ] {
            assert_eq!(normalize(name, None), None, "{name}");
        }
        // `ms` must be a whole word.
        assert_eq!(normalize("items_total", None), None);
    }

    #[test]
    fn a_unit_line_takes_precedence_over_the_name() {
        // A token in the name is replaced by the line's.
        assert_eq!(
            stored("rpc_ms", Some("seconds")),
            Some(("rpc_nanoseconds".into(), Some("nanoseconds"), 1e9))
        );
        // No token in the name: added when scaled, before the suffix.
        assert_eq!(
            stored("rpc_total", Some("seconds")),
            Some(("rpc_nanoseconds_total".into(), Some("nanoseconds"), 1e9))
        );
        // ... and the name kept when not.
        assert_eq!(
            stored("rpc", Some("bytes")),
            Some(("rpc".into(), Some("bytes"), 1.0))
        );
        // A line not in the table leaves the name to decide.
        assert_eq!(
            stored("rpc_ms", Some("furlongs")),
            Some(("rpc_nanoseconds".into(), Some("nanoseconds"), 1e6))
        );
        assert_eq!(stored("rpc", Some("furlongs")), None);
    }

    #[test]
    fn fallback_candidates_cover_every_rename() {
        let ms = fallback_candidates("rpc_ms");
        assert_eq!(ms[0], "rpc_nanoseconds", "the name's own rename first");
        assert!(ms.contains(&"rpc_bytes".to_string()));
        assert!(!ms.contains(&"rpc_ms".to_string()));
        let total = fallback_candidates("rpc_total");
        assert!(total.contains(&"rpc_nanoseconds_total".to_string()));
        assert!(total.contains(&"rpc_bytes_total".to_string()));
        assert!(fallback_candidates("rpc_bytes").contains(&"rpc_nanoseconds".to_string()));
        assert!(!fallback_candidates("rpc_bytes").contains(&"rpc_bytes".to_string()));
    }
}
