//! Tiny template renderer for `output_metric` strings.
//!
//! The catalogue's `output_metric` field is a `BTreeMap<String, String>`
//! holding label values that PromQL would have stamped onto the
//! resulting series — e.g. `{ __name__ = "{m}" }`. The values can
//! reference catalogue captures via `{name}` placeholders. The
//! `project::matrix` path renders each value once per query and
//! attaches the resulting metric labels to every output series.
//!
//! That single use site is the entire production scope of this module
//! today. Earlier in the migration this layer also rendered SQL
//! templates with richer transforms (`as_predicate`, `as_columns`,
//! `as_safe_col`, `as_seconds`); those are gone now that the wide-form
//! generator emits SQL directly. Resurrect from git if a future
//! consumer needs them.

use crate::{CaptureValue, Captures};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum InterpError {
    #[error("unknown placeholder `{0}` (no such capture)")]
    UnknownName(String),
    #[error("placeholder `{name}` of kind {kind} cannot render plainly; only ident/number/string/duration are supported here")]
    UnsupportedKind { name: String, kind: &'static str },
}

/// Substitute `{name}` placeholders in `template` with their capture
/// values. The recogniser is bounded: `\{(\w+)\}`. Anything outside
/// matches passes through unchanged so SQL-shaped strings (e.g. `{}`)
/// can appear in a value without confusing the substitution.
pub fn interpolate(template: &str, captures: &Captures) -> Result<String, InterpError> {
    let bytes = template.as_bytes();
    let mut out = String::with_capacity(template.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            let body_start = i + 1;
            let mut j = body_start;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'}' && j > body_start {
                let name = &template[body_start..j];
                let value = captures
                    .get(name)
                    .ok_or_else(|| InterpError::UnknownName(name.to_string()))?;
                let rendered = match value {
                    CaptureValue::Ident(s) => s.clone(),
                    CaptureValue::Number(n) => format!("{n}"),
                    CaptureValue::String(s) => s.clone(),
                    CaptureValue::Duration { seconds } => seconds.to_string(),
                    CaptureValue::Labels(_) => {
                        return Err(InterpError::UnsupportedKind {
                            name: name.to_string(),
                            kind: "labels",
                        });
                    }
                };
                out.push_str(&rendered);
                i = j + 1;
                continue;
            }
            // Unmatched `{` — pass through.
            out.push('{');
            i += 1;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn caps_with(name: &str, value: CaptureValue) -> Captures {
        let mut c = BTreeMap::new();
        c.insert(name.to_string(), value);
        c
    }

    #[test]
    fn ident_substitution() {
        let c = caps_with("m", CaptureValue::Ident("memory_total".into()));
        assert_eq!(interpolate("{m}", &c).unwrap(), "memory_total");
    }

    #[test]
    fn number_substitution() {
        let c = caps_with("q", CaptureValue::Number(0.99));
        assert_eq!(interpolate("p{q}", &c).unwrap(), "p0.99");
    }

    #[test]
    fn string_passes_through_value() {
        let c = caps_with("s", CaptureValue::String("hello".into()));
        assert_eq!(interpolate("[{s}]", &c).unwrap(), "[hello]");
    }

    #[test]
    fn duration_renders_seconds() {
        let c = caps_with("w", CaptureValue::Duration { seconds: 300 });
        assert_eq!(interpolate("{w}", &c).unwrap(), "300");
    }

    #[test]
    fn unmatched_brace_passes_through() {
        // `{}` (empty body) → not a placeholder; pass through verbatim.
        let c: Captures = BTreeMap::new();
        assert_eq!(interpolate("a {} b", &c).unwrap(), "a {} b");
    }

    #[test]
    fn unknown_name_errors() {
        let c: Captures = BTreeMap::new();
        let err = interpolate("{missing}", &c).unwrap_err();
        match err {
            InterpError::UnknownName(n) => assert_eq!(n, "missing"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn labels_capture_is_unsupported() {
        let c = caps_with("l", CaptureValue::Labels(Vec::new()));
        let err = interpolate("{l}", &c).unwrap_err();
        assert!(matches!(err, InterpError::UnsupportedKind { .. }));
    }
}
