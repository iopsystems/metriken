//! Archives of metriken metrics: a catalog of sources, tables, sealed
//! parquet segments, a WAL and caller rows (dendro's shape), read as one
//! `metriken_query::MetricsSource` per source.
//!
//! The only metriken crate that depends on dendro. See
//! `docs/journal/2026-09-28-high-cardinality-stack.md`.
//!
//! - [`catalog`]: what the reader needs from a container, and dendro's
//!   implementation of it.
//! - [`reader`]: the reader, [`ArchiveReader`].
//! - [`writer`] (feature `write`): the writer, [`ArchiveWriter`].

use std::collections::BTreeMap;
use std::sync::Arc;

pub mod catalog;
pub mod filter;
pub mod reader;
#[cfg(feature = "write")]
pub mod writer;

pub use catalog::{Catalog, DendroCatalog};
pub use filter::KeepMetrics;
pub use parquet::basic::{Compression, ZstdLevel};

/// The codec for every segment this crate encodes: the writer's sealed
/// segments, a copy's re-encoded tail, and the tail a reader rebuilds from
/// unsealed rows and holds in memory while it is open. zstd level 3: on
/// replayed recordings about half the bytes of LZ4 for about 4% more encode
/// time, with no measurable decode cost. A reader's tail stays resident, so
/// the smaller encoding is also the smaller footprint.
pub fn default_compression() -> Compression {
    Compression::ZSTD(ZstdLevel::try_new(3).expect("3 is a valid zstd level"))
}

/// The segment format's parquet writer properties with `compression`: what
/// a caller re-encoding a segment (a column projection) passes so the result
/// matches a sealed one.
pub fn segment_props(compression: Compression) -> parquet::file::properties::WriterProperties {
    metriken_segment::table::segment_writer_props()
        .into_builder()
        .set_compression(compression)
        .build()
}
pub use reader::{ArchiveReader, LabeledRecordings};
#[cfg(feature = "write")]
pub use writer::{ArchiveWriter, SourceRecorder, WriterConfig};

/// Opens an archive's catalog again, for a table first read after the
/// reader's own handle is gone.
pub type Reopen = Arc<dyn Fn() -> Result<Box<dyn Catalog>, String> + Send + Sync>;

/// Reads the caller rows kept under a table's name into a relabelling of its
/// columns: the hook for a container whose identity index lives in caller
/// rows (rezolus's `.rez` recordings made over its replication stream). A
/// table with an occupant stream needs none.
pub trait IndexRelabel: Send + Sync {
    fn relabel(
        &self,
        catalog: &dyn Catalog,
        source_id: i64,
        table: &str,
        first_row_ts: u64,
    ) -> Result<Arc<dyn metriken_query::ColumnRelabel>, String>;
}

/// A source whose tables are already in memory as parquet segments.
pub struct InMemorySource {
    /// What the source is called, for messages.
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub metadata: BTreeMap<String, String>,
    /// False when the source was recovered rather than cleanly finalized.
    pub complete: bool,
    /// `(table, segments)`, segments oldest first.
    pub tables: Vec<(String, Vec<Vec<u8>>)>,
}

/// Filesystem-safe directory name for a single recording, derived from its
/// `source` label (falls back to `"recording"`). The manifest — not the dir —
/// is authoritative for labels; this is only a human-readable tar path.
pub fn source_name(labels: &BTreeMap<String, String>) -> String {
    // `source` alone is not enough now that one archive routinely holds
    // several recordings: the documented multi-host capture
    // (`--endpoint web-01 --endpoint web-02`) leaves both recordings
    // `source=rezolus`, so both would display as "rezolus" — and unlike the
    // identical-labels case, that one draws no startup warning because the
    // label sets genuinely differ. Qualify with whichever of `arm`/`host` is
    // present.
    let base = labels
        .get("source")
        .map(String::as_str)
        .unwrap_or("recording");
    // `host` first, not `arm`. `--label` is global to a run — one `Vec`
    // applied to every endpoint — so within a single `record` invocation an
    // `arm` label is IDENTICAL across all its recordings, while `host` is the
    // per-endpoint one (it comes from each agent's own systeminfo). Preferring
    // `arm` would therefore discard the only qualifier that differs and
    // reproduce the collision this exists to fix:
    //
    //   record --endpoint web-01:4241 --endpoint web-02:4241 --label arm=redis
    //     arm-first  -> both recordings slug "rezolus-redis"
    //     host-first -> "rezolus-web-01" and "rezolus-web-02"
    //
    // `arm` stays as the fallback for archives `parquet combine` assembles
    // from separately-recorded runs, where it is the label that varies and
    // `host` may not be present at all.
    let qualifier = labels
        .get("host")
        .or_else(|| labels.get("arm"))
        .map(String::as_str);
    let base = match qualifier {
        Some(q) => format!("{base}-{q}"),
        None => base.to_string(),
    };
    let base = base.as_str();
    let slug: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if slug.is_empty() {
        "recording".to_string()
    } else {
        slug
    }
}

/// The sampler a `.rez` table key belongs to. V3 acquisition-group tables are
/// keyed `"<sampler>/<group>"`; a V2 (or windowless/default) sampler table's
/// key never contains `/` for every REGISTERED sampler of this build — see
/// `group_by_sampler`'s `sampler_of` and
/// `no_registered_sampler_name_contains_a_slash` — so it is its own sampler.
/// The manifest and filter unit stay the SAMPLER (the part before `/`), even
/// though the underlying table key is finer-grained.
///
/// Used by `rez_reader::RezReader` to group a sampler's tables for routing
/// and same-timeline union (Stage 4 Part C): two group tables sharing a
/// sampler answer a cross-group query together via a
/// `metriken_query::UnionMetricsSource`; two DIFFERENT samplers still refuse
/// a query spanning them.
///
/// `parquet filter --samplers` uses this on a v3 archive to drop a sampler's
/// group tables together — naming `cpu_usage` drops every `cpu_usage/<group>`
/// table, because the sampler is the unit an operator names. Exercised
/// directly by `table_sampler_tests`, by `rez_v3_writer`'s
/// `table_sampler_selects_a_samplers_group_tables_out_of_a_mixed_recording`
/// against a real recording, and end to end by
/// `filter_rez_v3_keeps_only_named_samplers`.
///
/// `parquet metadata` on a v3-native archive is a known, narrower instance of
/// the same display gap: it lists one line per GROUP table (`describe_v3_string`
/// in `parquet_tools::metadata`, driven by `RezDb::all_samplers`), so the
/// user-visible unit there is `cpu_usage/percpu`, not the sampler — this
/// function is exactly what closing that gap would group by, whenever it
/// lands; not done here to avoid growing this change further.
pub fn table_sampler(table_key: &str) -> &str {
    table_key.split('/').next().unwrap_or(table_key)
}
