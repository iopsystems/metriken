//! The unit a Prometheus series is in, from its name or its metric's
//! OpenMetrics `# UNIT` line.
//!
//! metriken-recorder's Prometheus converter stores every series under the
//! name the exporter serves it as, with the values the exporter serves, and
//! writes the unit [`unit()`] gives as the column's `unit` key. The unit is
//! the exporter's own: a `_seconds` series is in seconds and a `_ms` series
//! in milliseconds. [`UNITS`] is the vocabulary: each token a name or
//! `# UNIT` line is matched against, and the `unit` value written for it.

/// The suffixes removed from a name before its unit token is looked for.
pub const SUFFIXES: &[&str] = &["_bucket", "_sum", "_count", "_total"];

/// Each unit token and the `unit` storage key value written for it. A
/// token is matched whole and case-sensitively. Every value is the token
/// of some row, so a value is also a token.
///
/// The values are `seconds`, `milliseconds`, `microseconds`,
/// `nanoseconds`; `bytes`, `kilobytes`, `kibibytes`, `megabytes`,
/// `mebibytes`, `gigabytes`, `gibibytes`, `bits`; and `percent` (0 to
/// 100), `ratio`, `celsius`, `hertz`, `cores`.
pub const UNITS: &[(&str, &str)] = &[
    ("seconds", "seconds"),
    ("milliseconds", "milliseconds"),
    ("ms", "milliseconds"),
    ("msec", "milliseconds"),
    ("microseconds", "microseconds"),
    ("us", "microseconds"),
    ("nanoseconds", "nanoseconds"),
    ("ns", "nanoseconds"),
    ("bytes", "bytes"),
    ("kilobytes", "kilobytes"),
    ("kb", "kilobytes"),
    ("kibibytes", "kibibytes"),
    ("kib", "kibibytes"),
    ("megabytes", "megabytes"),
    ("mb", "megabytes"),
    ("mebibytes", "mebibytes"),
    ("mib", "mebibytes"),
    ("gigabytes", "gigabytes"),
    ("gb", "gigabytes"),
    ("gibibytes", "gibibytes"),
    ("gib", "gibibytes"),
    ("bits", "bits"),
    ("percent", "percent"),
    ("ratio", "ratio"),
    ("celsius", "celsius"),
    ("hertz", "hertz"),
    ("cores", "cores"),
];

/// The `unit` value for `token`, or `None` when it is not in [`UNITS`].
pub fn unit_of(token: &str) -> Option<&'static str> {
    UNITS
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

/// The `unit` value of a series an exporter serves as `name`, with
/// `unit_line` the value of its metric's OpenMetrics `# UNIT` line, if any.
///
/// A `unit_line` in [`UNITS`] decides it. Otherwise the name's token does:
/// the last `_`-separated word of `name` after removing one of
/// [`SUFFIXES`] (a name with no `_` there has none). `None` when neither is
/// in [`UNITS`], and for a `_count` or `_bucket` sample, whose value is a
/// count of observations whatever its metric's unit.
pub fn unit(name: &str, unit_line: Option<&str>) -> Option<&'static str> {
    let (stem, suffix) = split_suffix(name);
    if suffix == "_count" || suffix == "_bucket" {
        return None;
    }
    unit_line.and_then(unit_of).or_else(|| {
        stem.rsplit_once('_')
            .filter(|(head, _)| !head.is_empty())
            .and_then(|(_, token)| unit_of(token))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_token_gives_its_unit() {
        for (name, want) in [
            ("lat_seconds", "seconds"),
            ("lat_milliseconds", "milliseconds"),
            ("lat_ms", "milliseconds"),
            ("lat_msec", "milliseconds"),
            ("lat_microseconds", "microseconds"),
            ("lat_us", "microseconds"),
            ("lat_nanoseconds", "nanoseconds"),
            ("lat_ns", "nanoseconds"),
            ("mem_bytes", "bytes"),
            ("mem_kilobytes", "kilobytes"),
            ("mem_kb", "kilobytes"),
            ("mem_kibibytes", "kibibytes"),
            ("mem_kib", "kibibytes"),
            ("mem_megabytes", "megabytes"),
            ("mem_mb", "megabytes"),
            ("mem_mebibytes", "mebibytes"),
            ("mem_mib", "mebibytes"),
            ("mem_gigabytes", "gigabytes"),
            ("mem_gb", "gigabytes"),
            ("mem_gibibytes", "gibibytes"),
            ("mem_gib", "gibibytes"),
            ("net_bits", "bits"),
            ("cpu_percent", "percent"),
            ("hit_ratio", "ratio"),
            ("temp_celsius", "celsius"),
            ("clock_hertz", "hertz"),
            ("limit_cores", "cores"),
        ] {
            assert_eq!(unit(name, None), Some(want), "{name}");
        }
    }

    #[test]
    fn every_unit_is_a_token_of_itself() {
        for (_, value) in UNITS {
            assert_eq!(unit_of(value), Some(*value), "{value}");
        }
    }

    #[test]
    fn a_suffix_is_removed_and_a_count_has_no_unit() {
        assert_eq!(unit("rpc_ms_sum", None), Some("milliseconds"));
        assert_eq!(unit("cpu_seconds_total", None), Some("seconds"));
        assert_eq!(unit("rx_bits_total", None), Some("bits"));
        assert_eq!(unit("rpc_ms_count", None), None);
        assert_eq!(unit("rpc_ms_bucket", None), None);
        assert_eq!(unit("rpc_count", Some("seconds")), None);
    }

    #[test]
    fn a_name_without_a_unit_token_has_no_unit() {
        for name in [
            "requests_total",
            "items",
            "seconds",
            "_seconds",
            "x_total",
            "total",
            "items_total",
        ] {
            assert_eq!(unit(name, None), None, "{name}");
        }
    }

    #[test]
    fn a_unit_line_decides_over_the_name() {
        assert_eq!(unit("rpc_ms", Some("seconds")), Some("seconds"));
        assert_eq!(unit("rpc_total", Some("seconds")), Some("seconds"));
        assert_eq!(unit("rpc", Some("bytes")), Some("bytes"));
        assert_eq!(unit("rpc", Some("ms")), Some("milliseconds"));
        // A line not in the table leaves the name to decide.
        assert_eq!(unit("rpc_ms", Some("furlongs")), Some("milliseconds"));
        assert_eq!(unit("rpc", Some("furlongs")), None);
    }
}
