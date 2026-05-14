//! Catalogue of PromQL query templates and their SQL twins.
//!
//! The catalogue is a TOML registry: one entry per recognised PromQL
//! shape, with a compiled template that matches incoming queries and
//! extracts captures. Matched entries are routed through
//! `super::translate` to produce SQL, then through
//! `metriken-query-sql::DuckDbBackend::run_sql`, then through
//! `super::project` to produce a `QueryResult`.
//!
//! The catalogue is migration scaffolding. Once Rezolus emits SQL
//! natively, the whole layer can be deleted (along with `template`,
//! `translate`, `interp`, `project`).

use std::collections::BTreeMap;

use serde::Deserialize;

use super::template::{CompiledTemplate, Captures, TemplateError};

/// Output shape of a SQL twin — controls how the projector reads the
/// Arrow batches DuckDB returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputShape {
    /// `(t, value, [labels...])` rows projected into `QueryResult::Matrix`.
    Matrix,
    /// `(t, bucket_idx, count, p)` rows projected into
    /// `QueryResult::HistogramHeatmap`. The projector reconstructs
    /// `bucket_bounds` using the H2 bucket math directly.
    Heatmap,
}

fn default_output_shape() -> OutputShape {
    OutputShape::Matrix
}

/// One entry in the catalogue. Mirrors the `[[query]]` table in
/// `queries.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct CatalogueEntry {
    pub id: String,
    pub promql: String,
    /// `matrix` (default) or `heatmap`. For `heatmap`, `value_columns`
    /// / `label_columns` / `output_metric` are ignored — the projector
    /// uses the positional `(t, bucket_idx, count, p)` shape instead.
    #[serde(default = "default_output_shape")]
    pub output_shape: OutputShape,
    #[serde(default)]
    pub value_columns: Vec<String>,
    #[serde(default)]
    pub label_columns: Vec<String>,
    #[serde(default)]
    pub output_metric: BTreeMap<String, String>,
    /// Concrete query instantiations for the test harness, used when
    /// `promql` is a *template* (contains `${...}` placeholders) rather
    /// than a literal query. Captures extracted from each example query
    /// are forwarded to the SQL pipeline. Empty for literal-only entries.
    #[serde(default)]
    pub examples: Vec<GoldenExample>,
}

/// One concrete query instantiation for a templated catalogue entry.
#[derive(Debug, Clone, Deserialize)]
pub struct GoldenExample {
    pub id_suffix: String,
    pub query: String,
}

/// All catalogue entries, parsed from `queries.toml`.
///
/// Templates are compiled eagerly at `from_toml` time so that any
/// malformed template is rejected at startup, not on the first query
/// that happens to hit it. The compiled templates live in a parallel
/// `Vec` indexed identically to `entries`.
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
        Self::from_toml(include_str!("../../queries.toml"))
            .expect("invalid embedded queries.toml — should have been caught by the test harness")
    }

    pub fn entries(&self) -> &[CatalogueEntry] {
        &self.entries
    }

    /// Find the first entry whose compiled template matches `query`.
    /// Returns the matched entry plus the bag of named captures extracted
    /// from the query (empty for literal-only entries).
    ///
    /// Order matters: more-specific entries should appear before
    /// more-general ones in `queries.toml`, since matching is first-hit.
    pub fn lookup(&self, query: &str) -> Option<(&CatalogueEntry, Captures)> {
        for (entry, template) in self.entries.iter().zip(self.templates.iter()) {
            if let Some(captures) = template.match_query(query) {
                return Some((entry, captures));
            }
        }
        None
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
        assert_eq!(
            caps.get("m"),
            Some(&crate::harness::template::CaptureValue::Ident("memory_total".to_string()))
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
        assert!(cat.lookup("(nonsense + that + doesnt + match)").is_none());
    }
}
