//! SQL-template interpolator. Substitutes capture-bound placeholders in a
//! catalogue entry's `sql` field (and `output_metric` values) with the values
//! the matcher extracted from the incoming query.
//!
//! Placeholder syntax:
//! - `{name}` — substitute the capture named `name` per its kind:
//!   - `Ident(s)` → `s` verbatim (already validated as `[A-Za-z_]\w*`).
//!   - `Number(n)` → DuckDB-friendly numeric literal (no scientific exponent
//!     when round-trippable; `1.5e-3` style otherwise — whatever Rust's
//!     `Display` for `f64` produces).
//!   - `String(s)` → SQL string literal `'s'` with `'` doubled per ANSI SQL.
//!   - `Duration { seconds }` → integer second count.
//!   - `Labels(_)` → error; labels need an explicit transform.
//! - `{name:as_predicate}` — labels-only; emits a `WHERE`-clause fragment
//!   joining each `name OP value` predicate with ` AND `. Empty labels emit
//!   `TRUE`. Operators map: `=`/`!=` → SQL equality, `=~`/`!~` →
//!   `regexp_matches(col, 'pat')` / `NOT regexp_matches(col, 'pat')`.
//! - `{name:as_columns}` — labels-only; emits the comma-separated label
//!   names. Useful inside `PARTITION BY` / `GROUP BY` clauses.
//! - `{name:as_seconds}` — duration-only; emits the integer second count
//!   (same as the default for a Duration capture, kept explicit so callers
//!   can document intent).
//! - `{fixture_path}` — preserved special case; substitutes the runtime
//!   `data_source` path. Not a capture.
//!
//! The recogniser is bounded: `\{(\w+)(?::(\w+))?\}`. Anything that doesn't
//! fit (e.g. `{` followed by non-word characters, or an unclosed `{`) passes
//! through untouched — DuckDB's own parser will reject it later if it's truly
//! malformed. This keeps SQL like `SELECT {col} AS '...'` (empty label set
//! literal in some catalogue values) from being misread.

use metriken_query::{CaptureValue, Captures, LabelOp};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum InterpError {
    #[error("unknown placeholder `{0}` (no such capture)")]
    UnknownName(String),
    #[error("placeholder `{name}` is a {kind} capture; transform `{transform}` requires labels")]
    WrongTransformForKind {
        name: String,
        kind: &'static str,
        transform: String,
    },
    #[error("placeholder `{name}` of kind labels needs a transform (`as_predicate` or `as_columns`)")]
    MissingTransform { name: String },
    #[error("unknown transform `{0}`")]
    UnknownTransform(String),
    #[error("invalid label name `{0}` (must match [A-Za-z_]\\w*)")]
    InvalidLabelName(String),
}

/// Interpolate a SQL template with the given captures and data-source path.
pub fn interpolate(
    template: &str,
    captures: &Captures,
    data_source: &str,
) -> Result<String, InterpError> {
    let bytes = template.as_bytes();
    let mut out = String::with_capacity(template.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            // Scan a `{name}` or `{name:transform}` placeholder. Both name
            // and transform must be word-chars; anything else passes through.
            let body_start = i + 1;
            let mut j = body_start;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            let name_end = j;
            let mut transform_end = name_end;
            let transform = if j < bytes.len() && bytes[j] == b':' {
                let t_start = j + 1;
                let mut k = t_start;
                while k < bytes.len() && (bytes[k].is_ascii_alphanumeric() || bytes[k] == b'_') {
                    k += 1;
                }
                transform_end = k;
                Some(&template[t_start..k])
            } else {
                None
            };
            // We expect a closing `}` immediately after the name (or
            // transform). If not, treat the whole `{...` as literal.
            if transform_end < bytes.len() && bytes[transform_end] == b'}' && name_end > body_start
            {
                let name = &template[body_start..name_end];
                let rendered = render(name, transform, captures, data_source)?;
                out.push_str(&rendered);
                i = transform_end + 1;
                continue;
            }
            // Pass through the `{` as-is.
            out.push('{');
            i += 1;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    Ok(out)
}

fn render(
    name: &str,
    transform: Option<&str>,
    captures: &Captures,
    data_source: &str,
) -> Result<String, InterpError> {
    if name == "fixture_path" {
        return Ok(data_source.to_string());
    }
    let value = captures
        .get(name)
        .ok_or_else(|| InterpError::UnknownName(name.to_string()))?;
    match (value, transform) {
        // Default transform per kind.
        (CaptureValue::Ident(s), None) => Ok(s.clone()),
        (CaptureValue::Number(n), None) => Ok(format_number(*n)),
        (CaptureValue::String(s), None) => Ok(sql_string_literal(s)),
        (CaptureValue::Duration { seconds }, None) => Ok(seconds.to_string()),
        (CaptureValue::Duration { seconds }, Some("as_seconds")) => Ok(seconds.to_string()),
        (CaptureValue::Labels(_), None) => Err(InterpError::MissingTransform {
            name: name.to_string(),
        }),

        // Labels-only transforms.
        (CaptureValue::Labels(matchers), Some("as_predicate")) => {
            if matchers.is_empty() {
                Ok("TRUE".to_string())
            } else {
                let mut parts = Vec::with_capacity(matchers.len());
                for m in matchers {
                    if !is_safe_ident(&m.name) {
                        return Err(InterpError::InvalidLabelName(m.name.clone()));
                    }
                    let lit = sql_string_literal(&m.value);
                    parts.push(match m.op {
                        LabelOp::Eq => format!("{} = {lit}", m.name),
                        LabelOp::Ne => format!("{} != {lit}", m.name),
                        LabelOp::ReEq => format!("regexp_matches({}, {lit})", m.name),
                        LabelOp::ReNe => format!("NOT regexp_matches({}, {lit})", m.name),
                    });
                }
                Ok(parts.join(" AND "))
            }
        }
        (CaptureValue::Labels(matchers), Some("as_columns")) => {
            for m in matchers {
                if !is_safe_ident(&m.name) {
                    return Err(InterpError::InvalidLabelName(m.name.clone()));
                }
            }
            Ok(matchers
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>()
                .join(", "))
        }

        // Anything else is a kind/transform mismatch.
        (other, Some(t)) => Err(InterpError::WrongTransformForKind {
            name: name.to_string(),
            kind: kind_name(other),
            transform: t.to_string(),
        }),
    }
}

fn format_number(n: f64) -> String {
    // f64::Display emits short-as-possible round-trippable text; that's the
    // form the existing catalogue SQLs use for hard-coded literals (`0.5`,
    // `0.99`), so this matches the snapshot bytes after substitution.
    format!("{n}")
}

fn sql_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

fn is_safe_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn kind_name(v: &CaptureValue) -> &'static str {
    match v {
        CaptureValue::Ident(_) => "ident",
        CaptureValue::Number(_) => "number",
        CaptureValue::String(_) => "string",
        CaptureValue::Duration { .. } => "duration",
        CaptureValue::Labels(_) => "labels",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metriken_query::LabelMatcher;
    use std::collections::BTreeMap;

    fn caps() -> Captures {
        BTreeMap::new()
    }

    #[test]
    fn fixture_path_substitution_preserved() {
        let s = interpolate("SELECT * FROM read_parquet('{fixture_path}')", &caps(), "/tmp/x")
            .unwrap();
        assert_eq!(s, "SELECT * FROM read_parquet('/tmp/x')");
    }

    #[test]
    fn no_placeholders_passes_through_byte_identical() {
        let raw = "SELECT t, v FROM foo WHERE x = 1 ORDER BY t";
        assert_eq!(interpolate(raw, &caps(), "ignored").unwrap(), raw);
    }

    #[test]
    fn ident_capture_emits_verbatim() {
        let mut c = caps();
        c.insert("m".into(), CaptureValue::Ident("softirq".into()));
        let s = interpolate("FROM {m}", &c, "").unwrap();
        assert_eq!(s, "FROM softirq");
    }

    #[test]
    fn number_capture_emits_short_form() {
        let mut c = caps();
        c.insert("q".into(), CaptureValue::Number(0.5));
        let s = interpolate("hist({q})", &c, "").unwrap();
        assert_eq!(s, "hist(0.5)");

        c.insert("q".into(), CaptureValue::Number(0.99));
        let s = interpolate("hist({q})", &c, "").unwrap();
        assert_eq!(s, "hist(0.99)");
    }

    #[test]
    fn string_capture_quotes_and_escapes() {
        let mut c = caps();
        c.insert("k".into(), CaptureValue::String("hi".into()));
        assert_eq!(interpolate("WHERE kind = {k}", &c, "").unwrap(),
                   "WHERE kind = 'hi'");
        c.insert("k".into(), CaptureValue::String("it's".into()));
        assert_eq!(interpolate("WHERE kind = {k}", &c, "").unwrap(),
                   "WHERE kind = 'it''s'");
    }

    #[test]
    fn labels_as_predicate_handles_all_ops() {
        let mut c = caps();
        c.insert(
            "labels".into(),
            CaptureValue::Labels(vec![
                LabelMatcher { name: "a".into(), op: LabelOp::Eq,   value: "x".into() },
                LabelMatcher { name: "b".into(), op: LabelOp::Ne,   value: "y".into() },
                LabelMatcher { name: "c".into(), op: LabelOp::ReEq, value: "z.*".into() },
                LabelMatcher { name: "d".into(), op: LabelOp::ReNe, value: "w".into() },
            ]),
        );
        let s = interpolate("WHERE {labels:as_predicate}", &c, "").unwrap();
        assert_eq!(
            s,
            "WHERE a = 'x' AND b != 'y' AND regexp_matches(c, 'z.*') AND NOT regexp_matches(d, 'w')"
        );
    }

    #[test]
    fn labels_as_predicate_empty_emits_true() {
        let mut c = caps();
        c.insert("labels".into(), CaptureValue::Labels(Vec::new()));
        let s = interpolate("WHERE {labels:as_predicate}", &c, "").unwrap();
        assert_eq!(s, "WHERE TRUE");
    }

    #[test]
    fn labels_as_columns_emits_names() {
        let mut c = caps();
        c.insert(
            "labels".into(),
            CaptureValue::Labels(vec![
                LabelMatcher { name: "id".into(),    op: LabelOp::Eq, value: "x".into() },
                LabelMatcher { name: "state".into(), op: LabelOp::Eq, value: "y".into() },
            ]),
        );
        assert_eq!(
            interpolate("PARTITION BY {labels:as_columns}", &c, "").unwrap(),
            "PARTITION BY id, state"
        );
    }

    #[test]
    fn duration_capture_emits_integer_seconds_default() {
        let mut c = caps();
        c.insert("r".into(), CaptureValue::Duration { seconds: 300 });
        let s = interpolate("ROWS BETWEEN {r} PRECEDING AND CURRENT ROW", &c, "").unwrap();
        assert_eq!(s, "ROWS BETWEEN 300 PRECEDING AND CURRENT ROW");
    }

    #[test]
    fn duration_capture_as_seconds_transform() {
        let mut c = caps();
        c.insert("r".into(), CaptureValue::Duration { seconds: 5 });
        let s = interpolate("LAG(c, {r:as_seconds})", &c, "").unwrap();
        assert_eq!(s, "LAG(c, 5)");
    }

    #[test]
    fn unknown_name_errors() {
        let err = interpolate("{nope}", &caps(), "").unwrap_err();
        assert!(matches!(err, InterpError::UnknownName(_)));
    }

    #[test]
    fn labels_without_transform_errors() {
        let mut c = caps();
        c.insert("l".into(), CaptureValue::Labels(Vec::new()));
        let err = interpolate("{l}", &c, "").unwrap_err();
        assert!(matches!(err, InterpError::MissingTransform { .. }));
    }

    #[test]
    fn ident_with_label_transform_errors() {
        let mut c = caps();
        c.insert("m".into(), CaptureValue::Ident("foo".into()));
        let err = interpolate("{m:as_predicate}", &c, "").unwrap_err();
        assert!(matches!(err, InterpError::WrongTransformForKind { .. }));
    }

    #[test]
    fn malformed_braces_pass_through() {
        // `{}` with no name → leave as-is. Reasonable for output_metric maps
        // that legitimately contain `{}` JSON-like text in their values.
        let s = interpolate("{}", &caps(), "").unwrap();
        assert_eq!(s, "{}");
        // A naked `{` at end of string also passes through.
        let s = interpolate("trailing {", &caps(), "").unwrap();
        assert_eq!(s, "trailing {");
        // Brace followed by non-word char passes through.
        let s = interpolate("{ }", &caps(), "").unwrap();
        assert_eq!(s, "{ }");
    }
}
