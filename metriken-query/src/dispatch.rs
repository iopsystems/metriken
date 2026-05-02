//! Catalogue-driven runtime dispatcher.
//!
//! At runtime, `QueryEngine::query_range` consults the embedded query
//! catalogue and routes incoming PromQL according to each entry's `mode`:
//!
//! - `Off`     — PromQL only (default for entries without a SQL twin).
//! - `Shadow`  — Both engines run; results are compared; PromQL is returned.
//!               Disagreements are logged via the user-supplied
//!               [`DispatchObserver`] hook (so the host can metricise them).
//! - `Strict`  — Both run; disagreement is a hard error.
//! - `Primary` — SQL only.
//!
//! The PromQL surface stays unchanged — callers issue the same query strings
//! they always have. The dispatcher is opt-in: a `QueryEngine` constructed
//! without one behaves exactly like the pre-dispatcher engine.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::template::{CompiledTemplate, TemplateError};
use crate::{Captures, QueryResult};

/// Per-query lifecycle stage. Promotions are catalogue commits — `git log
/// queries.toml` is the canonical migration history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Off,
    Shadow,
    Strict,
    Primary,
}

/// Output shape of a SQL twin — controls how the backend projects rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputShape {
    /// `(t, value, [labels...])` rows projected into `QueryResult::Matrix`.
    Matrix,
    /// `(t, bucket_idx, count, p)` rows projected into
    /// `QueryResult::HistogramHeatmap`. Backend reconstructs `bucket_bounds`
    /// using the H2 bucket math directly.
    Heatmap,
}

fn default_output_shape() -> OutputShape {
    OutputShape::Matrix
}

/// One entry in the catalogue. Mirrors the `[[query]]` table in `queries.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct CatalogueEntry {
    pub id: String,
    #[serde(default = "default_mode")]
    pub mode: Mode,
    pub promql: String,
    #[serde(default)]
    pub sql: Option<String>,
    /// `matrix` (default) or `heatmap`. For `heatmap`, `value_columns` /
    /// `label_columns` / `output_metric` are ignored — the backend uses the
    /// positional `(t, bucket_idx, count, p)` shape instead.
    #[serde(default = "default_output_shape")]
    pub output_shape: OutputShape,
    #[serde(default)]
    pub value_columns: Vec<String>,
    #[serde(default)]
    pub label_columns: Vec<String>,
    #[serde(default)]
    pub output_metric: BTreeMap<String, String>,
    /// Concrete query instantiations for the golden test harness, used when
    /// `promql` is a *template* (contains `${...}` placeholders) rather than
    /// a literal query. Each example produces its own golden under
    /// `{entry.id}_{example.id_suffix}`. Empty for literal-only entries —
    /// the harness uses `promql` directly.
    #[serde(default)]
    pub examples: Vec<GoldenExample>,
    /// Test-time fields. Included so a single struct deserialises both the
    /// runtime path and the test harness.
    #[serde(default)]
    pub fixture: Option<String>,
    #[serde(default)]
    pub start: Option<f64>,
    #[serde(default)]
    pub end: Option<f64>,
    #[serde(default)]
    pub step: Option<f64>,
    #[serde(default)]
    pub description: Option<String>,
}

/// One concrete query instantiation for a templated catalogue entry. Drives
/// the golden harness: each example produces its own snapshot, and the
/// captures extracted from `query` are forwarded to the SQL backend.
#[derive(Debug, Clone, Deserialize)]
pub struct GoldenExample {
    pub id_suffix: String,
    pub query: String,
}

fn default_mode() -> Mode {
    Mode::Off
}

/// All catalogue entries, parsed from `queries.toml`.
///
/// Templates are compiled eagerly at `from_toml` time so that any malformed
/// template is rejected at startup, not on the first query that happens to
/// hit it. The compiled templates live in a parallel `Vec` indexed identically
/// to `entries`.
#[derive(Debug, Clone)]
pub struct Catalogue {
    entries: Vec<CatalogueEntry>,
    templates: Vec<CompiledTemplate>,
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogueError {
    #[error("toml parse error: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("template `{id}` failed to compile: {source}")]
    Template {
        id: String,
        #[source]
        source: TemplateError,
    },
}

impl Catalogue {
    /// Parse the catalogue text (`queries.toml` content) into entries.
    /// Compiles each entry's `promql` template and surfaces any template
    /// parse error along with the offending entry id.
    pub fn from_toml(text: &str) -> Result<Self, CatalogueError> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(rename = "query")]
            entries: Vec<CatalogueEntry>,
        }
        let raw: Raw = toml::from_str(text)?;
        let mut templates = Vec::with_capacity(raw.entries.len());
        for e in &raw.entries {
            let t = CompiledTemplate::parse(&e.promql).map_err(|source| {
                CatalogueError::Template {
                    id: e.id.clone(),
                    source,
                }
            })?;
            templates.push(t);
        }
        Ok(Self {
            entries: raw.entries,
            templates,
        })
    }

    /// The version of the catalogue compiled into this crate.
    pub fn embedded() -> Self {
        Self::from_toml(include_str!("../queries.toml"))
            .expect("invalid embedded queries.toml — should have been caught by the test harness")
    }

    pub fn entries(&self) -> &[CatalogueEntry] {
        &self.entries
    }

    /// Find the first entry whose compiled template matches `query`. Returns
    /// the matched entry plus the bag of named captures extracted from the
    /// query (empty for literal-only entries).
    ///
    /// Order matters: more-specific entries should appear before more-general
    /// ones in `queries.toml`, since matching is first-hit.
    pub fn lookup(&self, query: &str) -> Option<(&CatalogueEntry, Captures)> {
        for (entry, template) in self.entries.iter().zip(self.templates.iter()) {
            if let Some(captures) = template.match_query(query) {
                return Some((entry, captures));
            }
        }
        None
    }
}

/// Backend that knows how to execute the SQL twin of a catalogue entry.
///
/// `metriken-query-sql` provides the canonical implementation against an
/// embedded DuckDB. A test or alternative backend can implement this trait
/// to plug a different executor (or a stub) without dragging DuckDB into
/// `metriken-query`.
///
/// `data_source` is the parquet path (or glob) the backend reads from —
/// substituted into the `{fixture_path}` placeholder in the catalogue's
/// SQL template. The same placeholder name is used at runtime and at test
/// time; semantically it's "the parquet file you're querying."
pub trait SqlBackend: Send + Sync {
    fn run(
        &self,
        entry: &CatalogueEntry,
        captures: &Captures,
        data_source: &str,
        start: f64,
        end: f64,
        step: f64,
    ) -> Result<QueryResult, SqlError>;
}

#[derive(Debug, thiserror::Error)]
pub enum SqlError {
    #[error("SQL backend error: {0}")]
    Backend(String),
}

/// Hook the host wires up to observe shadow-mode disagreements and per-query
/// dispatch telemetry. Default impls are no-ops so the dispatcher works
/// without instrumentation.
pub trait DispatchObserver: Send + Sync {
    /// Called when shadow- or strict-mode comparison detects a divergence
    /// between PromQL and SQL outputs. `Diff` carries the canonical JSON for
    /// each side so the observer can serialise it for richer reports.
    fn on_diff(&self, entry: &CatalogueEntry, _diff: &Diff) {
        let _ = entry;
    }

    /// Called once per dispatched query (catalogue match), regardless of
    /// mode or diff. `promql_ms` is `None` when mode = Primary (PromQL was
    /// skipped); `sql_ms` is `None` when mode = Off (SQL was skipped) or the
    /// SQL backend errored out before producing a result.
    fn on_dispatch(
        &self,
        entry: &CatalogueEntry,
        mode: Mode,
        promql_ms: Option<f64>,
        sql_ms: Option<f64>,
    ) {
        let _ = (entry, mode, promql_ms, sql_ms);
    }
}

/// A diff between PromQL and SQL output for a single catalogue entry, as
/// canonical JSON. Holds enough for the observer to log or metricise; the
/// observer can serialise it for richer reports.
#[derive(Debug)]
pub struct Diff {
    pub promql: Value,
    pub sql: Value,
}

/// No-op observer used when the host doesn't supply one.
pub struct NoopObserver;
impl DispatchObserver for NoopObserver {}

/// Canonicalise a `QueryResult` into a deterministic `serde_json::Value`:
/// every `Object` is rebuilt with sorted keys, and `result` arrays of
/// objects-with-a-`metric`-field are sorted by the canonical JSON of that
/// metric. This is the same canonicalisation the golden test harness uses,
/// so test-time and runtime diffs are bit-comparable.
pub fn canonicalise(result: &QueryResult) -> Value {
    let value = serde_json::to_value(result).expect("serialize QueryResult");
    let mut canon = sort_keys(value);
    sort_series_arrays(&mut canon);
    canon
}

fn sort_keys(value: Value) -> Value {
    match value {
        Value::Object(obj) => {
            let mut entries: Vec<_> = obj.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut out = Map::new();
            for (k, v) in entries {
                out.insert(k, sort_keys(v));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(sort_keys).collect()),
        other => other,
    }
}

fn sort_series_arrays(value: &mut Value) {
    if let Value::Object(obj) = value {
        if let Some(Value::Array(arr)) = obj.get_mut("result") {
            if arr
                .iter()
                .all(|e| e.as_object().map(|o| o.contains_key("metric")).unwrap_or(false))
            {
                arr.sort_by(|a, b| {
                    let ka = a.get("metric").map(|m| m.to_string()).unwrap_or_default();
                    let kb = b.get("metric").map(|m| m.to_string()).unwrap_or_default();
                    ka.cmp(&kb)
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalogue_parses() {
        let cat = Catalogue::embedded();
        assert!(!cat.entries().is_empty(), "expected at least one entry");
    }

    #[test]
    fn lookup_finds_known_query() {
        let cat = Catalogue::embedded();
        // `memory_total` matches the templated `gauge_bare` entry with
        // `m = memory_total`. Both the entry id and the capture binding
        // are part of the contract.
        let (entry, caps) = cat.lookup("memory_total").expect("gauge_bare match");
        assert_eq!(entry.id, "gauge_bare");
        assert_eq!(entry.mode, Mode::Shadow);
        assert_eq!(
            caps.get("m"),
            Some(&crate::CaptureValue::Ident("memory_total".to_string()))
        );
    }

    #[test]
    fn lookup_is_whitespace_tolerant() {
        let cat = Catalogue::embedded();
        let (a, _) = cat.lookup("sum by (id) (irate(cpu_usage[5m]))").unwrap();
        let (b, _) = cat
            .lookup("  sum by (id)  (irate(cpu_usage[5m]))  ")
            .unwrap();
        assert_eq!(a.id, b.id);
    }

    #[test]
    fn lookup_misses_unknown_query() {
        let cat = Catalogue::embedded();
        // A free-form expression that doesn't fit any entry's template
        // shape. (`gauge_bare` is now `${m:ident}` which would absorb a
        // bare identifier — using a parenthesised form ensures no entry
        // matches.)
        assert!(cat.lookup("(nonsense + that + doesnt + match)").is_none());
    }

    #[test]
    fn modes_round_trip_through_toml() {
        let toml = r#"
            [[query]]
            id = "x"
            mode = "shadow"
            promql = "x"

            [[query]]
            id = "y"
            mode = "primary"
            promql = "y"

            [[query]]
            id = "z"
            promql = "z"
        "#;
        let cat = Catalogue::from_toml(toml).expect("parse");
        let modes: Vec<_> = cat.entries.iter().map(|e| e.mode).collect();
        assert_eq!(modes, vec![Mode::Shadow, Mode::Primary, Mode::Off]);
    }
}
