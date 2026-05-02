//! Capture-group templating for the catalogue matcher.
//!
//! A catalogue entry's `promql` field is parsed once at startup into a
//! [`CompiledTemplate`] — a sequence of literal text and named capture
//! placeholders. At lookup time, an incoming PromQL query is matched against
//! each entry's compiled template; on a successful match, the matcher returns
//! a [`Captures`] map binding each placeholder name to the value extracted
//! from the query.
//!
//! Placeholders use a `${name}` or `${name:kind}` syntax. The `$` prefix
//! avoids collisions with PromQL syntax (PromQL has no `$` token). Supported
//! kinds:
//!
//! - `ident`   — bare identifier (default if no kind is given).
//! - `number`  — numeric literal, parses as f64.
//! - `string`  — PromQL string literal; the matcher consumes the surrounding
//!   `"..."` quotes and the capture value is the (unescaped) inner text. So a
//!   template fragment `kind=${k:string}` matches a query fragment `kind="hi"`
//!   (no quotes on the template side — the placeholder owns them).
//! - `duration` — PromQL duration literal `<digits><s|m|h|d|w>`. Captured as
//!   a count of seconds. Common in `[5m]` / `[5s]` window suffixes — one
//!   templated entry covers both Rust dashboards (`[5m]`) and JSON service
//!   templates (`[5s]`). Pair with the `as_seconds` SQL transform to emit an
//!   integer second count.
//! - `labels`  — full label-set inner text (between the surrounding `{` and
//!   `}`, which are part of the template literal). The capture value is a
//!   sorted-by-label-name list of `(name, op, value)` triples.
//!
//! Backward compatibility: a template with no `${...}` placeholders parses
//! to a single `Literal` part and matches by whitespace-tolerant equality —
//! identical behaviour to the pre-templating matcher.

use std::collections::BTreeMap;

use thiserror::Error;

/// Map from capture-name to extracted value, populated when a query matches a
/// templated catalogue entry. Empty for literal-only entries.
pub type Captures = BTreeMap<String, CaptureValue>;

/// One placeholder kind. Each kind has a strict grammar for what it consumes
/// from the query; anything that doesn't match is a non-match (the dispatcher
/// falls through to the next entry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureKind {
    /// Bare identifier `[a-zA-Z_][a-zA-Z0-9_]*`.
    Ident,
    /// Numeric literal — integer, decimal, or scientific notation.
    Number,
    /// PromQL string literal — the matcher consumes the surrounding `"`
    /// quotes; the captured value is the inner (unescaped) text. PromQL also
    /// allows backtick strings, but Rezolus's `format!()` paths only emit
    /// `"..."`.
    String,
    /// Label-set inner text. The surrounding `{` and `}` are part of the
    /// template literal; this captures only what's between them. The captured
    /// value is parsed into a sorted list of `(name, op, value)` triples so
    /// `{a="x", b="y"}` and `{b="y", a="x"}` produce identical captures.
    Labels,
    /// PromQL duration literal — `<digits><s|m|h|d|w>`. Captured as a count
    /// of seconds. `5m` → 300, `5s` → 5, `1h` → 3600.
    Duration,
}

/// One part of a parsed template — either a chunk of literal text or a named
/// placeholder that consumes a span of the query during matching.
#[derive(Debug, Clone)]
pub enum TemplatePart {
    /// Literal text, already whitespace-normalised at parse time.
    Literal(String),
    /// Named capture with a kind that determines its grammar.
    Capture { name: String, kind: CaptureKind },
}

/// One label predicate inside a `${labels:labels}` capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelMatcher {
    pub name: String,
    pub op: LabelOp,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelOp {
    /// `=`
    Eq,
    /// `!=`
    Ne,
    /// `=~`
    ReEq,
    /// `!~`
    ReNe,
}

impl LabelOp {
    /// PromQL textual form (also what we emit when round-tripping).
    pub fn as_promql(&self) -> &'static str {
        match self {
            LabelOp::Eq => "=",
            LabelOp::Ne => "!=",
            LabelOp::ReEq => "=~",
            LabelOp::ReNe => "!~",
        }
    }
}

/// One extracted capture, typed by what the placeholder said it should be.
#[derive(Debug, Clone, PartialEq)]
pub enum CaptureValue {
    Ident(String),
    Number(f64),
    /// Inner text of a quoted string (the matcher already stripped quotes).
    String(String),
    /// Label-set inner text, parsed and sorted by label name.
    Labels(Vec<LabelMatcher>),
    /// Duration in seconds — `5m` → 300, `5s` → 5.
    Duration { seconds: u64 },
}

/// A parsed catalogue template. Built once per entry at catalogue load time.
#[derive(Debug, Clone)]
pub struct CompiledTemplate {
    parts: Vec<TemplatePart>,
}

#[derive(Debug, Error)]
pub enum TemplateError {
    #[error("unterminated placeholder starting at position {0}")]
    UnterminatedPlaceholder(usize),
    #[error("empty placeholder at position {0}")]
    EmptyPlaceholder(usize),
    #[error("invalid placeholder name `{0}` (must match [A-Za-z_][A-Za-z0-9_]*)")]
    BadName(String),
    #[error("unknown capture kind `{0}` (expected ident, number, string, duration, labels)")]
    UnknownKind(String),
    #[error("duplicate capture name `{0}` in template")]
    DuplicateName(String),
}

impl CompiledTemplate {
    /// Parse a template string. Whitespace inside literal parts is normalised
    /// the same way as queries (runs of whitespace collapse to single spaces).
    /// Errors surface here rather than at match time so `Catalogue::from_toml`
    /// fails loud at startup if a template is malformed.
    pub fn parse(template: &str) -> Result<Self, TemplateError> {
        let mut parts: Vec<TemplatePart> = Vec::new();
        let mut literal = String::new();
        let mut seen_names: Vec<String> = Vec::new();
        let bytes = template.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            // Look for `${` start of a placeholder.
            if i + 1 < bytes.len() && bytes[i] == b'$' && bytes[i + 1] == b'{' {
                // Flush current literal (after normalisation).
                if !literal.is_empty() {
                    parts.push(TemplatePart::Literal(collapse_ws(&literal)));
                    literal.clear();
                }
                // Find matching `}`.
                let placeholder_start = i;
                let body_start = i + 2;
                let body_end = match memchr(b'}', &bytes[body_start..]) {
                    Some(off) => body_start + off,
                    None => return Err(TemplateError::UnterminatedPlaceholder(placeholder_start)),
                };
                let body = &template[body_start..body_end];
                if body.is_empty() {
                    return Err(TemplateError::EmptyPlaceholder(placeholder_start));
                }
                let (name, kind) = match body.find(':') {
                    Some(colon) => {
                        let n = &body[..colon];
                        let k = &body[colon + 1..];
                        (n, parse_kind(k)?)
                    }
                    None => (body, CaptureKind::Ident),
                };
                if !is_ident(name) {
                    return Err(TemplateError::BadName(name.to_string()));
                }
                if seen_names.iter().any(|n| n == name) {
                    return Err(TemplateError::DuplicateName(name.to_string()));
                }
                seen_names.push(name.to_string());
                parts.push(TemplatePart::Capture {
                    name: name.to_string(),
                    kind,
                });
                i = body_end + 1; // skip past the `}`
            } else {
                literal.push(bytes[i] as char);
                i += 1;
            }
        }
        if !literal.is_empty() {
            parts.push(TemplatePart::Literal(collapse_ws(&literal)));
        }
        // Trim leading WS on the FIRST literal and trailing WS on the LAST,
        // so the matcher (which sees a fully-trimmed query) doesn't have to
        // skip border whitespace. Boundary spaces between captures and
        // intermediate literals are preserved by collapse_ws.
        if let Some(TemplatePart::Literal(s)) = parts.first_mut() {
            if s.starts_with(' ') {
                s.remove(0);
            }
        }
        if let Some(TemplatePart::Literal(s)) = parts.last_mut() {
            if s.ends_with(' ') {
                s.pop();
            }
        }
        Ok(CompiledTemplate { parts })
    }

    /// True iff this template has at least one capture placeholder. Used by
    /// callers that want to skip the captures plumbing for purely literal
    /// entries (a small optimisation).
    pub fn has_captures(&self) -> bool {
        self.parts
            .iter()
            .any(|p| matches!(p, TemplatePart::Capture { .. }))
    }

    /// Parts of this template, in declaration order. Exposed for diagnostics
    /// and tests; callers normally use `match_query` instead.
    pub fn parts(&self) -> &[TemplatePart] {
        &self.parts
    }

    /// Try to match `query` against this template. Returns the bound captures
    /// on success, or `None` on any mismatch. `query` is whitespace-normalised
    /// before matching, the same way template literals were at parse time.
    pub fn match_query(&self, query: &str) -> Option<Captures> {
        let normalised = normalise(query);
        let q = normalised.as_bytes();
        let mut pos = 0usize;
        let mut captures: Captures = BTreeMap::new();

        for (idx, part) in self.parts.iter().enumerate() {
            match part {
                TemplatePart::Literal(lit) => {
                    // PromQL is whitespace-insensitive at token boundaries:
                    // `, ` (Rezolus's `format!()` output) and `,` (typed-by-
                    // hand) are the same query. So when the template literal
                    // contains a single space, allow the query to either have
                    // matching whitespace or none. Non-space chars must match
                    // exactly.
                    let lit_bytes = lit.as_bytes();
                    let mut li = 0usize;
                    while li < lit_bytes.len() {
                        let lc = lit_bytes[li];
                        if lc == b' ' {
                            // Optional whitespace: consume any number of WS
                            // chars in the query (already normalised, so they
                            // can only be single spaces).
                            while pos < q.len() && q[pos] == b' ' {
                                pos += 1;
                            }
                            li += 1;
                        } else {
                            if pos >= q.len() || q[pos] != lc {
                                return None;
                            }
                            pos += 1;
                            li += 1;
                        }
                    }
                }
                TemplatePart::Capture { name, kind } => {
                    // Determine the next-literal terminator (used by Number /
                    // Ident scans that need to know where to stop). For Labels
                    // and String the terminator is determined by the kind's
                    // own grammar.
                    let next_literal = self.parts.get(idx + 1).and_then(|p| match p {
                        TemplatePart::Literal(s) => Some(s.as_str()),
                        _ => None,
                    });
                    let (consumed, value) = match kind {
                        CaptureKind::Ident => scan_ident(&q[pos..])?,
                        CaptureKind::Number => scan_number(&q[pos..])?,
                        CaptureKind::String => scan_string(&q[pos..])?,
                        CaptureKind::Duration => scan_duration(&q[pos..])?,
                        CaptureKind::Labels => scan_labels(&q[pos..], next_literal)?,
                    };
                    captures.insert(name.clone(), value);
                    pos += consumed;
                }
            }
        }
        if pos == q.len() {
            Some(captures)
        } else {
            None
        }
    }
}

fn parse_kind(s: &str) -> Result<CaptureKind, TemplateError> {
    match s {
        "ident" => Ok(CaptureKind::Ident),
        "number" => Ok(CaptureKind::Number),
        "string" => Ok(CaptureKind::String),
        "duration" => Ok(CaptureKind::Duration),
        "labels" => Ok(CaptureKind::Labels),
        other => Err(TemplateError::UnknownKind(other.to_string())),
    }
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn memchr(needle: u8, haystack: &[u8]) -> Option<usize> {
    haystack.iter().position(|&b| b == needle)
}

/// Whitespace normalisation for a *query* string: collapse runs of ASCII
/// whitespace into single spaces; trim leading/trailing whitespace.
/// Same semantics the pre-templating matcher used in `dispatch.rs`.
pub fn normalise(s: &str) -> String {
    let mut out = collapse_ws(s);
    if out.starts_with(' ') {
        out.remove(0);
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Whitespace collapse for a template *chunk*: same as `normalise` but
/// preserves any single space at the start/end. The space between two
/// captures (`${q}, ${m}` → chunk `, `) carries the boundary information that
/// `normalise` would strip — without it the matcher would advance past the
/// `,` in the query, then try to scan an identifier from a position that
/// still starts with whitespace, and bail.
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(ch);
            last_space = false;
        }
    }
    out
}

// --- per-kind scanners. Each takes the bytes-from-cursor and returns the
//     number of bytes consumed plus the captured value, or None on mismatch.

fn scan_ident(q: &[u8]) -> Option<(usize, CaptureValue)> {
    // Accept the Rezolus metric-name superset: a leading [A-Za-z_] followed by
    // [A-Za-z0-9_/:] — `/` is used by Rezolus to namespace per-source metrics
    // (`redis/total_commands_processed`) and `:` is the PromQL convention for
    // recording-rule names (`service:request_latency:p99`). Without these the
    // template engine couldn't capture real production metric names.
    if q.is_empty() {
        return None;
    }
    let first = q[0];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return None;
    }
    let mut end = 1;
    while end < q.len()
        && (q[end].is_ascii_alphanumeric()
            || q[end] == b'_'
            || q[end] == b'/'
            || q[end] == b':')
    {
        end += 1;
    }
    let s = std::str::from_utf8(&q[..end]).ok()?.to_string();
    Some((end, CaptureValue::Ident(s)))
}

fn scan_number(q: &[u8]) -> Option<(usize, CaptureValue)> {
    if q.is_empty() {
        return None;
    }
    // Number := [+-]? digits ('.' digits)? ([eE][+-]?digits)?
    // We're permissive but bounded — anything that f64::from_str accepts is
    // a number.
    let mut end = 0;
    if matches!(q.first(), Some(b'+') | Some(b'-')) {
        end += 1;
    }
    let mut saw_digit = false;
    while end < q.len() && q[end].is_ascii_digit() {
        end += 1;
        saw_digit = true;
    }
    if end < q.len() && q[end] == b'.' {
        end += 1;
        while end < q.len() && q[end].is_ascii_digit() {
            end += 1;
            saw_digit = true;
        }
    }
    if !saw_digit {
        return None;
    }
    if end < q.len() && (q[end] == b'e' || q[end] == b'E') {
        end += 1;
        if matches!(q.get(end), Some(b'+') | Some(b'-')) {
            end += 1;
        }
        let exp_start = end;
        while end < q.len() && q[end].is_ascii_digit() {
            end += 1;
        }
        if end == exp_start {
            // dangling exponent marker — bail
            return None;
        }
    }
    let s = std::str::from_utf8(&q[..end]).ok()?;
    let n: f64 = s.parse().ok()?;
    Some((end, CaptureValue::Number(n)))
}

fn scan_duration(q: &[u8]) -> Option<(usize, CaptureValue)> {
    // PromQL duration: `<digits><unit>` where unit is one of s, m, h, d, w.
    // (The Rezolus dashboards only emit `[5m]` and `[5s]`; we accept the
    // common units for completeness.)
    if q.is_empty() {
        return None;
    }
    let mut end = 0;
    while end < q.len() && q[end].is_ascii_digit() {
        end += 1;
    }
    if end == 0 {
        return None;
    }
    if end >= q.len() {
        return None;
    }
    let unit_seconds: u64 = match q[end] {
        b's' => 1,
        b'm' => 60,
        b'h' => 3_600,
        b'd' => 86_400,
        b'w' => 604_800,
        _ => return None,
    };
    let digits = std::str::from_utf8(&q[..end]).ok()?;
    let n: u64 = digits.parse().ok()?;
    let seconds = n.checked_mul(unit_seconds)?;
    Some((end + 1, CaptureValue::Duration { seconds }))
}

fn scan_string(q: &[u8]) -> Option<(usize, CaptureValue)> {
    // Consume `"..."`. PromQL allows `\"` and `\\` escapes; we honour the
    // common ones. The captured value is the unescaped inner text.
    if q.first() != Some(&b'"') {
        return None;
    }
    let mut i = 1;
    let mut out = String::new();
    while i < q.len() {
        match q[i] {
            b'"' => {
                return Some((i + 1, CaptureValue::String(out)));
            }
            b'\\' if i + 1 < q.len() => {
                let next = q[i + 1];
                let ch = match next {
                    b'n' => '\n',
                    b't' => '\t',
                    b'r' => '\r',
                    b'\\' => '\\',
                    b'"' => '"',
                    b'\'' => '\'',
                    other => other as char,
                };
                out.push(ch);
                i += 2;
            }
            other => {
                out.push(other as char);
                i += 1;
            }
        }
    }
    // Unterminated string.
    None
}

fn scan_labels(
    q: &[u8],
    next_literal: Option<&str>,
) -> Option<(usize, CaptureValue)> {
    // The labels capture lives BETWEEN the surrounding `{` and `}` — those
    // braces are part of the template literal. So we know the next character
    // in the template after this capture is `}`. We scan until we see `}` at
    // the top level (respecting quoted strings, in case a label value
    // contains a `}` — uncommon but allowed).
    //
    // The `next_literal` hint is unused here — included for symmetry with
    // other scanners and in case a future change wants to use it for
    // disambiguation.
    let _ = next_literal;
    let mut i = 0;
    let mut in_string = false;
    let mut esc = false;
    while i < q.len() {
        if in_string {
            if esc {
                esc = false;
            } else if q[i] == b'\\' {
                esc = true;
            } else if q[i] == b'"' {
                in_string = false;
            }
        } else {
            match q[i] {
                b'"' => in_string = true,
                b'}' => break,
                _ => {}
            }
        }
        i += 1;
    }
    if i >= q.len() {
        // Ran off the end without finding `}` — mismatch.
        return None;
    }
    let inner = std::str::from_utf8(&q[..i]).ok()?;
    let mut matchers = parse_label_set(inner)?;
    matchers.sort_by(|a, b| a.name.cmp(&b.name));
    Some((i, CaptureValue::Labels(matchers)))
}

/// Parse the inner text of a label set (between the braces).
/// Empty (or all-whitespace) input → empty matcher list.
fn parse_label_set(s: &str) -> Option<Vec<LabelMatcher>> {
    let s = s.trim();
    if s.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Skip leading whitespace.
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        // Label name: ident.
        let name_start = i;
        if !(bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
            return None;
        }
        i += 1;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        let name = std::str::from_utf8(&bytes[name_start..i]).ok()?.to_string();
        // Skip whitespace.
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        // Operator.
        let op = if i + 1 < bytes.len() && bytes[i] == b'=' && bytes[i + 1] == b'~' {
            i += 2;
            LabelOp::ReEq
        } else if i + 1 < bytes.len() && bytes[i] == b'!' && bytes[i + 1] == b'~' {
            i += 2;
            LabelOp::ReNe
        } else if i + 1 < bytes.len() && bytes[i] == b'!' && bytes[i + 1] == b'=' {
            i += 2;
            LabelOp::Ne
        } else if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            LabelOp::Eq
        } else {
            return None;
        };
        // Skip whitespace.
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        // Value (quoted string).
        let (consumed, value) = scan_string(&bytes[i..])?;
        let value = match value {
            CaptureValue::String(v) => v,
            _ => unreachable!("scan_string always returns String"),
        };
        i += consumed;
        out.push(LabelMatcher { name, op, value });
        // Skip whitespace, then optional comma.
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b',' {
            i += 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(t: &str) -> CompiledTemplate {
        CompiledTemplate::parse(t).expect("parse")
    }

    #[test]
    fn literal_only_template_is_a_single_part() {
        let c = parse("memory_total");
        assert_eq!(c.parts().len(), 1);
        assert!(matches!(&c.parts()[0], TemplatePart::Literal(s) if s == "memory_total"));
        assert!(!c.has_captures());
    }

    #[test]
    fn literal_only_template_matches_self() {
        let c = parse("sum by (id) (irate(cpu_usage[5m]))");
        let caps = c.match_query("sum by (id) (irate(cpu_usage[5m]))").unwrap();
        assert!(caps.is_empty());
    }

    #[test]
    fn template_space_in_literal_matches_no_space_in_query() {
        // Rezolus's `format!()` emits `histogram_quantile(0.5, m)` with a
        // space after the comma; humans typing curl commands omit it. Both
        // should hit the same templated entry.
        let c = parse("histogram_quantile(${q:number}, ${m:ident})");
        let with_space = c.match_query("histogram_quantile(0.5, scheduler_runqueue_latency)");
        let no_space = c.match_query("histogram_quantile(0.5,scheduler_runqueue_latency)");
        assert!(with_space.is_some(), "with-space variant must match");
        assert!(no_space.is_some(), "no-space variant must match");
        assert_eq!(with_space.as_ref().unwrap().get("q"), no_space.as_ref().unwrap().get("q"));
        assert_eq!(with_space.as_ref().unwrap().get("m"), no_space.as_ref().unwrap().get("m"));
    }

    #[test]
    fn whitespace_tolerance_is_preserved() {
        let c = parse("sum by (id) (irate(cpu_usage[5m]))");
        let caps = c
            .match_query("  sum by (id)  (irate(cpu_usage[5m]))  ")
            .unwrap();
        assert!(caps.is_empty());
    }

    #[test]
    fn unrelated_query_does_not_match() {
        let c = parse("memory_total");
        assert!(c.match_query("memory_available").is_none());
    }

    #[test]
    fn ident_capture_extracts_metric_name() {
        let c = parse("irate(${m}[5m])");
        let caps = c.match_query("irate(softirq[5m])").unwrap();
        assert_eq!(caps.get("m"), Some(&CaptureValue::Ident("softirq".into())));
    }

    #[test]
    fn number_capture_extracts_quantile() {
        let c = parse("histogram_quantile(${q:number}, request_latency)");
        let caps = c
            .match_query("histogram_quantile(0.5, request_latency)")
            .unwrap();
        assert_eq!(caps.get("q"), Some(&CaptureValue::Number(0.5)));

        let caps = c
            .match_query("histogram_quantile(0.99, request_latency)")
            .unwrap();
        assert_eq!(caps.get("q"), Some(&CaptureValue::Number(0.99)));
    }

    #[test]
    fn number_capture_handles_int_and_scientific() {
        let c = parse("foo(${n:number})");
        for (q, want) in [
            ("foo(1)", 1.0),
            ("foo(0)", 0.0),
            ("foo(1.5)", 1.5),
            ("foo(1e3)", 1000.0),
            ("foo(1.5e-3)", 0.0015),
            ("foo(-2.5)", -2.5),
        ] {
            let caps = c.match_query(q).expect(q);
            assert_eq!(caps.get("n"), Some(&CaptureValue::Number(want)), "{q}");
        }
    }

    #[test]
    fn number_capture_rejects_garbage() {
        let c = parse("foo(${n:number})");
        assert!(c.match_query("foo(abc)").is_none());
        assert!(c.match_query("foo(1.5x)").is_none());
        assert!(c.match_query("foo()").is_none());
    }

    #[test]
    fn string_capture_owns_the_quotes() {
        // Template has NO surrounding `"` — `${k:string}` consumes them.
        let c = parse("softirq{kind=${k:string}}");
        let caps = c.match_query(r#"softirq{kind="hi"}"#).unwrap();
        assert_eq!(caps.get("k"), Some(&CaptureValue::String("hi".into())));

        let caps = c.match_query(r#"softirq{kind="net_rx"}"#).unwrap();
        assert_eq!(caps.get("k"), Some(&CaptureValue::String("net_rx".into())));
    }

    #[test]
    fn string_capture_rejects_unquoted() {
        let c = parse("softirq{kind=${k:string}}");
        assert!(c.match_query("softirq{kind=hi}").is_none());
    }

    #[test]
    fn string_capture_handles_metric_names_with_slashes() {
        // Production wrinkle: `redis/total_commands_processed` has a `/`.
        // It only ever appears as a label value in Rezolus, not as a metric
        // name reaching `${m:ident}`.
        let c = parse("foo{source=${s:string}}");
        let caps = c
            .match_query(r#"foo{source="redis/total_commands_processed"}"#)
            .unwrap();
        assert_eq!(
            caps.get("s"),
            Some(&CaptureValue::String("redis/total_commands_processed".into()))
        );
    }

    #[test]
    fn duration_capture_extracts_seconds_5m() {
        let c = parse("rate(${m:ident}[${r:duration}])");
        let caps = c.match_query("rate(requests[5m])").unwrap();
        assert_eq!(caps.get("r"), Some(&CaptureValue::Duration { seconds: 300 }));
    }

    #[test]
    fn duration_capture_extracts_seconds_5s() {
        let c = parse("rate(${m:ident}[${r:duration}])");
        let caps = c.match_query("rate(requests[5s])").unwrap();
        assert_eq!(caps.get("r"), Some(&CaptureValue::Duration { seconds: 5 }));
    }

    #[test]
    fn duration_capture_handles_all_units() {
        let c = parse("foo(${r:duration})");
        for (q, want_secs) in [
            ("foo(30s)", 30u64),
            ("foo(2m)", 120),
            ("foo(1h)", 3600),
            ("foo(1d)", 86_400),
            ("foo(2w)", 1_209_600),
        ] {
            let caps = c.match_query(q).expect(q);
            assert_eq!(
                caps.get("r"),
                Some(&CaptureValue::Duration { seconds: want_secs }),
                "{q}"
            );
        }
    }

    #[test]
    fn duration_capture_rejects_garbage() {
        let c = parse("foo(${r:duration})");
        assert!(c.match_query("foo(5x)").is_none());
        assert!(c.match_query("foo(m)").is_none());
        assert!(c.match_query("foo(5)").is_none());
    }

    #[test]
    fn labels_capture_single_label() {
        let c = parse("irate(softirq{${labels:labels}}[5m])");
        let caps = c
            .match_query(r#"irate(softirq{kind="hi"}[5m])"#)
            .unwrap();
        let labels = match caps.get("labels").unwrap() {
            CaptureValue::Labels(l) => l,
            _ => panic!("not labels"),
        };
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].name, "kind");
        assert_eq!(labels[0].op, LabelOp::Eq);
        assert_eq!(labels[0].value, "hi");
    }

    #[test]
    fn labels_capture_is_permutation_invariant() {
        let c = parse("foo{${labels:labels}}");
        let a = c.match_query(r#"foo{a="1", b="2"}"#).unwrap();
        let b = c.match_query(r#"foo{b="2", a="1"}"#).unwrap();
        assert_eq!(a.get("labels"), b.get("labels"));
    }

    #[test]
    fn labels_capture_supports_all_operators() {
        let c = parse("foo{${labels:labels}}");
        let caps = c
            .match_query(r#"foo{a="x", b!="y", c=~"z", d!~"w"}"#)
            .unwrap();
        let labels = match caps.get("labels").unwrap() {
            CaptureValue::Labels(l) => l.clone(),
            _ => panic!(),
        };
        assert_eq!(labels.len(), 4);
        assert_eq!(labels[0].op, LabelOp::Eq);
        assert_eq!(labels[1].op, LabelOp::Ne);
        assert_eq!(labels[2].op, LabelOp::ReEq);
        assert_eq!(labels[3].op, LabelOp::ReNe);
    }

    #[test]
    fn labels_capture_empty_set() {
        let c = parse("foo{${labels:labels}}");
        let caps = c.match_query("foo{}").unwrap();
        let labels = match caps.get("labels").unwrap() {
            CaptureValue::Labels(l) => l,
            _ => panic!(),
        };
        assert!(labels.is_empty());
    }

    #[test]
    fn parse_error_unterminated_placeholder() {
        let err = CompiledTemplate::parse("foo${bar").unwrap_err();
        assert!(matches!(err, TemplateError::UnterminatedPlaceholder(_)));
    }

    #[test]
    fn parse_error_empty_placeholder() {
        let err = CompiledTemplate::parse("foo${}").unwrap_err();
        assert!(matches!(err, TemplateError::EmptyPlaceholder(_)));
    }

    #[test]
    fn parse_error_unknown_kind() {
        let err = CompiledTemplate::parse("foo${x:bogus}").unwrap_err();
        assert!(matches!(err, TemplateError::UnknownKind(_)));
    }

    #[test]
    fn parse_error_duplicate_name() {
        let err = CompiledTemplate::parse("foo${a} bar${a}").unwrap_err();
        assert!(matches!(err, TemplateError::DuplicateName(_)));
    }

    #[test]
    fn parse_error_bad_name() {
        let err = CompiledTemplate::parse("foo${1bad}").unwrap_err();
        assert!(matches!(err, TemplateError::BadName(_)));
    }

    #[test]
    fn dollar_without_brace_is_literal() {
        // No `{` after `$` — kept as-is. (Rezolus never emits `$`, so this is
        // unlikely to come up, but the lexer shouldn't choke.)
        let c = parse("price$5");
        assert!(c.match_query("price$5").is_some());
    }

    #[test]
    fn template_with_two_captures_extracts_both() {
        let c = parse(r#"sum by (${by:ident}) (irate(${m:ident}{kind=${k:string}}[5m]))"#);
        let caps = c
            .match_query(r#"sum by (id) (irate(softirq{kind="net_rx"}[5m]))"#)
            .unwrap();
        assert_eq!(caps.get("by"), Some(&CaptureValue::Ident("id".into())));
        assert_eq!(caps.get("m"), Some(&CaptureValue::Ident("softirq".into())));
        assert_eq!(caps.get("k"), Some(&CaptureValue::String("net_rx".into())));
    }

    #[test]
    fn end_of_query_must_align_with_end_of_template() {
        let c = parse("memory_total");
        assert!(c.match_query("memory_total trailing").is_none());
        let c = parse("foo(${n:number})");
        assert!(c.match_query("foo(1) extra").is_none());
    }

    #[test]
    fn every_existing_catalogue_entry_compiles_and_matches_a_concrete_query() {
        // For literal-only entries: each should match its own promql verbatim.
        // For templated entries: each example query should match.
        let raw = include_str!("../queries.toml");
        let value: toml::Value = toml::from_str(raw).expect("parse toml");
        let queries = value
            .get("query")
            .and_then(|v| v.as_array())
            .expect("[[query]] array");
        let mut count = 0;
        for q in queries {
            let promql = q
                .get("promql")
                .and_then(|v| v.as_str())
                .expect("promql field");
            let id = q.get("id").and_then(|v| v.as_str()).unwrap_or("?");
            let compiled = CompiledTemplate::parse(promql)
                .unwrap_or_else(|e| panic!("compile {id}: {e}"));

            if compiled.has_captures() {
                // Templated entry — examples are required so we can verify
                // the template matches at least one concrete query.
                let examples = q
                    .get("examples")
                    .and_then(|v| v.as_array())
                    .unwrap_or_else(|| panic!("templated entry {id} needs examples"));
                assert!(
                    !examples.is_empty(),
                    "templated entry {id}: examples must be non-empty"
                );
                for ex in examples {
                    let q_text = ex
                        .get("query")
                        .and_then(|v| v.as_str())
                        .unwrap_or_else(|| panic!("{id} example missing 'query'"));
                    compiled
                        .match_query(q_text)
                        .unwrap_or_else(|| panic!("{id}: template did not match `{q_text}`"));
                }
            } else {
                let caps = compiled
                    .match_query(promql)
                    .unwrap_or_else(|| panic!("match {id} against itself"));
                assert!(caps.is_empty(), "{id}: literal-only should have no captures");
            }
            count += 1;
        }
        assert!(count >= 18, "only checked {count} entries — did the file load?");
    }
}
