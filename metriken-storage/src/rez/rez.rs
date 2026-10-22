//! The `.rez` per-sampler archive, read only: an uncompressed tar of
//! `manifest.json` plus one parquet table per sampler (v1/v2), or a SQLite
//! file (v3, [`super::rez_sqlite`]).
//!
//! A table is one or more parquet *segments* (schema v2), so a tar archive
//! was either written whole (`manifest.json` first, then
//! `<dir>/<sampler>.parquet`) or streamed (segments interleaved with
//! checkpoint manifests, the last of which may be missing entirely on an
//! unclean kill). Reading tolerates a truncated tail; see
//! `read_archive_reader`.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

/// The newest `.rez` tar manifest schema version (the last one written).
pub const REZ_SCHEMA_VERSION: u32 = 2;
/// Highest manifest schema version this build can read. v1 shipped without a
/// forward gate (which is what makes a downgraded/compacted v1-shaped manifest
/// readable by old binaries); v2 adds one, because gates cannot be retrofitted.
pub const REZ_MAX_SUPPORTED_VERSION: u32 = 2;
/// Manifest filename inside the tar.
pub const REZ_MANIFEST_NAME: &str = "manifest.json";
/// The segment format's sidecar column names (`crate::table`).
pub use crate::table::{WALL_OFFSET_COLUMN, WINDOW_BEGIN_COLUMN, WINDOW_WIDTH_COLUMN};

/// Top-level `.rez` manifest (`manifest.json`): a bag of label-tagged recordings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RezManifest {
    pub version: u32,
    pub recordings: Vec<RezRecording>,
}

/// One recording = one endpoint on one host = a label set + its per-sampler tables.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RezRecording {
    /// Filesystem-safe directory holding this recording's parquet tables in the tar.
    pub dir: String,
    /// Arbitrary label set: `source`, `host` (from systeminfo), user `--label k=v`.
    pub labels: BTreeMap<String, String>,
    /// Per-recording metadata: the existing `parquet_metadata` keys
    /// (`systeminfo`, `descriptions`, `sampling_interval_ms`, ...).
    pub metadata: BTreeMap<String, String>,
    /// True iff this recording was cleanly finalized. Checkpoint manifests
    /// never set it, so an archive recovered from one presents as incomplete.
    /// It lives per-recording, not on the manifest: `parquet combine` merges
    /// recordings from different archives, where one recovered + one clean
    /// recording has no truthful top-level value. Absent (v1) means false.
    #[serde(default)]
    pub complete: bool,
    /// Wall-clock reading (ns since epoch) at recording start. Row timestamps
    /// are `anchor + monotonic elapsed`, so this pins the timeline to wall time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_anchor_wall_ns: Option<u64>,
    /// `(anchored_ts, wall_minus_anchored_ns)` observations, one per checkpoint:
    /// an at-a-glance clock-drift summary that needs no table decode. Each is a
    /// projection of some row's `:wall_offset` — the newest row in that
    /// checkpoint's seal batch — so the series never contradicts the tables.
    ///
    /// Not guaranteed sorted: the observation is a per-batch maximum, so an
    /// age-sealed slow sampler can append a timestamp older than one a
    /// fast-sampler batch already contributed. Consumers that need order should
    /// sort; consumers that need per-row precision should read the column.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clock_offsets: Vec<(u64, i64)>,
    pub tables: Vec<RezTableIndex>,
}

/// One entry in the manifest's table index. A table is one or more parquet
/// segments; `rows`/`cadence_ns` are totals across them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RezTableIndex {
    pub sampler: String,
    /// v1 single-file name. Set only when the table is a single segment (so v1
    /// readers can still open compacted/atomically-written archives); never
    /// serialized as `null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Segment file names in segment order. Absent on v1 manifests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// Column names. Defaulted because checkpoint manifests omit it: it is
    /// O(total columns) and reaches 100 KB+ on cgroup-heavy tables, and
    /// recovery does not need it to load segments.
    #[serde(default)]
    pub columns: Vec<String>,
    pub rows: u64,
    /// Observed mean row interval (ns); `None` when fewer than 2 rows.
    pub cadence_ns: Option<u64>,
}

impl RezTableIndex {
    /// The table's segment files in order: `files` when present, else the v1
    /// single `file`, else empty (a malformed index naming no data).
    pub fn segment_files(&self) -> Vec<&str> {
        if !self.files.is_empty() {
            self.files.iter().map(String::as_str).collect()
        } else {
            self.file.as_deref().into_iter().collect()
        }
    }
}

// The wide table and its parquet decoding are the segment format's
// (`crate::table`, see metriken's
// docs/journal/2026-09-28-high-cardinality-stack.md); re-exported under the
// names rezolus uses.
pub use crate::table::{
    read_table_parquet, Column as RezColumn, Table as RezTable, Values as RezValues,
};

type RezError = Box<dyn std::error::Error>;

use std::io::Read;
use std::path::Path;

/// One recording's raw per-sampler parquet bytes (for building readers).
pub struct RecordingBytes {
    pub dir: String,
    pub labels: BTreeMap<String, String>,
    pub metadata: BTreeMap<String, String>,
    /// False when the recording was recovered from a checkpoint rather than
    /// cleanly finalized — data after its last row may be missing. Always true
    /// for v1 archives, which predate unclean-kill recovery.
    pub complete: bool,
    /// `(sampler, segment_bytes)` in manifest order; segments in segment order.
    pub tables: Vec<(String, Vec<Vec<u8>>)>,
}

/// Take the parquet blobs `manifest` references out of `blobs`.
///
/// Two phases on purpose. The presence check runs first and mutates nothing,
/// so a manifest that does not resolve leaves `blobs` intact for the next
/// (older) manifest to try. Only then are the bytes *moved* out rather than
/// cloned — a `.rez` is read whole into memory, so copying would double peak
/// RSS on a large archive.
fn resolve(
    manifest: &RezManifest,
    blobs: &mut HashMap<String, Vec<u8>>,
) -> Result<Vec<RecordingBytes>, RezError> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for rec in &manifest.recordings {
        for idx in &rec.tables {
            for seg in idx.segment_files() {
                let path = format!("{}/{}", rec.dir, seg);
                if !blobs.contains_key(&path) {
                    return Err(format!("missing table file {path}").into());
                }
                // Two index entries naming one file would make the move below
                // ill-defined; reject before touching anything.
                if !seen.insert(path.clone()) {
                    return Err(format!("table file {path} is referenced twice").into());
                }
            }
        }
    }

    // v1 predates unclean-kill recovery: every v1 writer emitted the whole
    // archive at once, so a v1 recording is complete and the flag is not read.
    let interpret_complete = manifest.version >= 2;
    let mut recordings = Vec::with_capacity(manifest.recordings.len());
    for rec in &manifest.recordings {
        let mut tables = Vec::with_capacity(rec.tables.len());
        for idx in &rec.tables {
            let mut segments = Vec::new();
            for seg in idx.segment_files() {
                let path = format!("{}/{}", rec.dir, seg);
                segments.push(
                    blobs
                        .remove(&path)
                        .ok_or_else(|| format!("missing table file {path}"))?,
                );
            }
            tables.push((idx.sampler.clone(), segments));
        }
        recordings.push(RecordingBytes {
            dir: rec.dir.clone(),
            labels: rec.labels.clone(),
            metadata: rec.metadata.clone(),
            complete: !interpret_complete || rec.complete,
            tables,
        });
    }
    Ok(recordings)
}

/// Read a `.rez` from any reader into its manifest + per-recording raw parquet
/// bytes (unlike `read_archive`, which decodes tables into `RezTable`s).
///
/// Tar iteration is truncation-tolerant, because an unclean kill (SIGKILL,
/// power loss) is a supported way for a streaming recording to end. The rules,
/// stated precisely because the `tar` crate does not error where one would
/// expect: an entry counts only if the bytes read equal the header's declared
/// size; any tar error, short read, or unparseable manifest ends iteration.
/// Recovery then uses the newest manifest whose files are all present — the
/// newest one written may reference segments the truncated tail ate, since a
/// checkpoint manifest is always followed by more segments.
pub fn read_archive_reader<R: std::io::Read>(
    reader: R,
) -> Result<(RezManifest, Vec<RecordingBytes>), RezError> {
    let mut archive = tar::Archive::new(reader);
    // Every manifest in the archive, oldest first: the streaming writer appended
    // a checkpoint manifest after each seal batch (duplicate tar names are
    // legal and this reader is order-agnostic).
    let mut manifests: Vec<RezManifest> = Vec::new();
    let mut blobs: HashMap<String, Vec<u8>> = HashMap::new();

    for entry in archive.entries()? {
        // A truncated header errors here; everything before it is still good.
        let Ok(mut entry) = entry else { break };
        let size = entry.size();
        let name = match entry.path() {
            Ok(p) => p.to_string_lossy().into_owned(),
            Err(_) => break,
        };
        let mut buf = Vec::new();
        // A mid-data truncation yields the entry `Ok` and reads a *silently*
        // short buffer (the body is a `Take` over a reader already at EOF), so
        // the length check — not the `Result` — is what catches it.
        match entry.read_to_end(&mut buf) {
            Ok(read) if read as u64 == size => {}
            _ => break,
        }
        if name == REZ_MANIFEST_NAME {
            match serde_json::from_slice::<RezManifest>(&buf) {
                Ok(m) => {
                    if m.version > REZ_MAX_SUPPORTED_VERSION {
                        return Err(format!(
                            "unsupported .rez manifest version {} (this build reads up to \
                             version {}); upgrade rezolus to read this archive",
                            m.version, REZ_MAX_SUPPORTED_VERSION
                        )
                        .into());
                    }
                    manifests.push(m);
                }
                // A half-persisted checkpoint manifest ends iteration; recovery
                // falls back to the previous one.
                Err(_) => break,
            }
        } else if name.ends_with(".parquet") {
            blobs.insert(name, buf);
        }
    }

    if manifests.is_empty() {
        return Err("missing manifest.json".into());
    }
    // Newest resolvable manifest wins; report the newest failure if none does.
    let mut newest_err: Option<RezError> = None;
    while let Some(mut manifest) = manifests.pop() {
        match resolve(&manifest, &mut blobs) {
            Ok(recordings) => {
                // Normalize `complete` on the manifest too, by the same rule
                // `resolve` applies to `RecordingBytes` (v1 recordings are
                // complete by construction; the absent flag is not read), so a
                // caller that reads the manifest sees the same answer as one
                // that reads the recordings.
                if manifest.version < 2 {
                    for rec in &mut manifest.recordings {
                        rec.complete = true;
                    }
                }
                return Ok((manifest, recordings));
            }
            Err(e) => newest_err = newest_err.or(Some(e)),
        }
    }
    Err(newest_err.unwrap_or_else(|| "missing manifest.json".into()))
}

/// Read a `.rez` archive at `path` into manifest + per-recording raw bytes.
pub fn read_archive_bytes(path: &Path) -> Result<(RezManifest, Vec<RecordingBytes>), RezError> {
    read_archive_reader(std::fs::File::open(path)?)
}

/// True if `reader` yields a v2 `.rez` archive: an uncompressed tar containing
/// a top-level `manifest.json` member. Distinguishes `.rez` from the A/B
/// tarball (which has `ab.json` + root-level parquets, no `manifest.json`) and
/// from a bare parquet (not a tar). Consumes the reader.
///
/// This only recognizes the v2 tar container; a v3 SQLite archive is not a
/// tar, so this correctly (and deliberately) reports `false` for it. Every
/// `.rez` consumer that needs to recognize both containers should call
/// [`detect_rez_format`] instead — this function remains as the tar-only
/// primitive `detect_rez_format` builds on, and for tests that specifically
/// want to exercise v2 tar sniffing (e.g. of a truncated/partial stream).
pub fn is_rez_reader<R: std::io::Read>(reader: R) -> Result<bool, RezError> {
    let mut archive = tar::Archive::new(reader);
    let entries = match archive.entries() {
        Ok(e) => e,
        Err(_) => return Ok(false),
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => return Ok(false),
        };
        if entry.path()?.to_string_lossy() == REZ_MANIFEST_NAME {
            return Ok(true);
        }
    }
    Ok(false)
}

/// True if the file at `path` is a `.rez` archive (by content, not extension).
pub fn is_rez_path(path: &Path) -> Result<bool, RezError> {
    is_rez_reader(std::fs::File::open(path)?)
}

/// Which container a `.rez` path holds. v1/v2 are tar archives; v3 is SQLite.
///
/// [`super::open`] dispatches on this: v1/v2 are read eagerly through
/// `read_archive_bytes`, v3 through its SQLite catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RezFormat {
    V3Sqlite,
    V2Tar,
    NotRez,
}

/// SQLite's file header: bytes 0..16 of every database it creates.
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// Detect a `.rez` container by content, not extension.
///
/// The SQLite check goes first because it is a 16-byte read, while the tar
/// sniff walks entries looking for `manifest.json`. A file shorter than the
/// magic simply falls through. This sits in front of `is_rez_path` without
/// changing it: `is_rez_path`/`is_rez_reader` still only recognize the v2 tar
/// container. A v3 SQLite file is not a tar, so `is_rez_path` correctly (and
/// unchanged) reports `false` for it; callers that need to recognize both
/// containers call `detect_rez_format`.
/// Which container an in-memory archive holds — [`detect_rez_format`] for
/// bytes, which is what a caller holding an upload has.
///
/// Shares `SQLITE_MAGIC` and `is_rez_reader` with the path version rather than
/// re-deriving either: a browser and the CLI disagreeing about what a file IS
/// would be the worst possible place for a second rule.
pub fn detect_rez_format_bytes(bytes: &[u8]) -> RezFormat {
    if looks_like_v3(bytes) {
        return RezFormat::V3Sqlite;
    }
    match is_rez_reader(std::io::Cursor::new(bytes)) {
        Ok(true) => RezFormat::V2Tar,
        _ => RezFormat::NotRez,
    }
}

/// True when `bytes` starts with SQLite's file header — a v3 `.rez` container.
pub fn looks_like_v3(bytes: &[u8]) -> bool {
    bytes.len() >= SQLITE_MAGIC.len() && &bytes[..SQLITE_MAGIC.len()] == SQLITE_MAGIC
}

pub fn detect_rez_format(path: &Path) -> Result<RezFormat, RezError> {
    let mut file = std::fs::File::open(path)?;
    let mut header = [0u8; 16];
    let is_sqlite = match file.read_exact(&mut header) {
        Ok(()) => &header == SQLITE_MAGIC,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => false,
        Err(e) => return Err(e.into()),
    };
    if is_sqlite {
        return Ok(RezFormat::V3Sqlite);
    }
    if is_rez_path(path)? {
        Ok(RezFormat::V2Tar)
    } else {
        Ok(RezFormat::NotRez)
    }
}

/// The sampler a `.rez` table key belongs to. V3 acquisition-group tables are
/// keyed `"<sampler>/<group>"`; a V2 (or windowless/default) sampler table's
/// key never contains `/` for any sampler rezolus registers, so it is its
/// own sampler.
/// The manifest and filter unit stay the SAMPLER (the part before `/`), even
/// though the underlying table key is finer-grained.
///
/// Used when routing a query (`ArchiveReader::owners_of` and
/// `metriken-query`'s archive routing) to tell two group tables of one
/// sampler, which a query can union, from the same sampler in two
/// recordings, which it cannot.
///
/// A filter that names a sampler selects every `<sampler>/<group>` table of
/// it, because the sampler is the unit an operator names. Exercised by
/// `table_sampler_tests`.
pub fn table_sampler(table_key: &str) -> &str {
    table_key.split('/').next().unwrap_or(table_key)
}

#[cfg(test)]
mod table_sampler_tests {
    use super::table_sampler;

    #[test]
    fn splits_a_group_table_key_at_the_first_slash() {
        assert_eq!(table_sampler("cpu_usage/percpu"), "cpu_usage");
        // A group name may itself contain further structure, but only the
        // FIRST segment is the sampler — the manifest/filter unit stays
        // coarse even if a group name were to contain another `/`.
        assert_eq!(table_sampler("cpu_usage/acq_0"), "cpu_usage");
    }

    #[test]
    fn a_plain_v1_v2_sampler_key_is_its_own_sampler() {
        assert_eq!(table_sampler("cpu_usage"), "cpu_usage");
        assert_eq!(table_sampler("blockio_requests"), "blockio_requests");
        assert_eq!(table_sampler("unattributed"), "unattributed");
    }

    /// `filter --samplers <name>` (once V3 SQLite support lands — currently
    /// out of scope, see `parquet_tools::filter`'s explicit "not yet
    /// supported" branch) would select every table whose `table_sampler`
    /// equals the requested name. This pins that selection rule directly
    /// against a realistic mixed table-key set, so the rule is proven correct
    /// independent of when the CLI plumbing for it lands.
    #[test]
    fn table_sampler_selects_every_group_table_of_one_sampler() {
        let tables = [
            "cpu_usage/percpu",
            "cpu_usage/aggregate",
            "blockio_requests/latency",
            "scheduler",
        ];
        let selected: Vec<&str> = tables
            .iter()
            .copied()
            .filter(|t| table_sampler(t) == "cpu_usage")
            .collect();
        assert_eq!(selected, vec!["cpu_usage/percpu", "cpu_usage/aggregate"]);
    }
}

/// Extract the `hostname` string from a systeminfo JSON blob, if present.
pub fn host_from_systeminfo(systeminfo_json: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(systeminfo_json)
        .ok()?
        .get("hostname")?
        .as_str()
        .map(|s| s.to_string())
}

/// Filesystem-safe directory name for a single recording, derived from its
/// `source` label (falls back to `"recording"`). The manifest — not the dir —
/// is authoritative for labels; this is only a human-readable tar path.
pub fn recording_dir_slug(labels: &BTreeMap<String, String>) -> String {
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

#[cfg(test)]
mod builder_tests {
    use super::*;
    use crate::builder::{
        cells_approx_bytes, Cell, CellValue, TableBuilder, CELL_OVERHEAD_BYTES,
        HISTOGRAM_BUCKET_BYTES, VALUE_SLOT_BYTES, WINDOW_SLOT_BYTES,
    };
    use crate::table::{table_to_batch, write_table_parquet};
    use crate::window::Window;
    use std::collections::HashMap;

    fn cmeta(sampler: &str) -> HashMap<String, String> {
        [("sampler".to_string(), sampler.to_string())]
            .into_iter()
            .collect()
    }

    fn hist(gp: u8, mvp: u8) -> histogram::Histogram {
        let mut h = histogram::Histogram::new(gp, mvp).unwrap();
        h.increment(1_000).unwrap();
        h
    }

    fn cell<'a>(
        name: &'a str,
        metadata: &'a HashMap<String, String>,
        window: Option<Window>,
        value: CellValue<'a>,
    ) -> Cell<'a> {
        Cell {
            name,
            metadata,
            window,
            value,
        }
    }

    // The seal threshold is byte-first, so `push_row` must maintain the byte
    // count itself: a row's cost is dominated by histogram buckets, which row
    // counts do not see at all (one gp=7/mvp=64 cell is ~58 KB).
    #[test]
    fn push_row_accumulates_approx_bytes() {
        let md = cmeta("s");
        let mut b = TableBuilder::new("s".to_string());
        assert_eq!(b.approx_bytes(), 0, "a fresh builder accounts nothing");

        b.push_row(1_000, 0, &[cell("0", &md, None, CellValue::Counter(1))]);
        let after_scalar = b.approx_bytes();
        assert_eq!(
            after_scalar, CELL_OVERHEAD_BYTES,
            "one scalar cell is its value slot plus its window slot"
        );

        let h = hist(7, 64);
        let buckets = h.as_slice().len();
        assert_eq!(buckets, h.config().total_buckets());
        b.push_row(2_000, 0, &[cell("1", &md, None, CellValue::Histogram(&h))]);
        assert_eq!(
            b.approx_bytes() - after_scalar,
            CELL_OVERHEAD_BYTES + buckets * HISTOGRAM_BUCKET_BYTES,
            "one histogram cell is the per-cell overhead plus its buckets"
        );
    }

    // `cells_approx_bytes` is what lets a container with no builder seal at
    // the same row a buffering one does, which is only true if it returns
    // exactly what `push_row` charges. Asserted against the builder itself over
    // a row carrying every cell shape, rather than by restating the arithmetic
    // — a restatement drifts in lockstep with the bug it should catch.
    #[test]
    fn cells_approx_bytes_matches_what_push_row_accounts() {
        let md = cmeta("s");
        let h = hist(7, 64);
        let cells = [
            cell("c", &md, None, CellValue::Counter(1)),
            cell("g", &md, None, CellValue::Gauge(1)),
            cell("h", &md, None, CellValue::Histogram(&h)),
        ];

        let mut b = TableBuilder::new("s".to_string());
        b.push_row(1_000, 0, &cells);
        assert_eq!(
            cells_approx_bytes(&cells),
            b.approx_bytes(),
            "the builderless estimate must equal what push_row charges"
        );

        // Again on a second row, so an estimate that only matches while columns
        // are being created cannot pass.
        let before = b.approx_bytes();
        b.push_row(2_000, 0, &cells);
        assert_eq!(cells_approx_bytes(&cells), b.approx_bytes() - before);
    }

    // Regression: `approx_bytes` bounds resident memory, and it used to charge
    // a scalar cell only its value slot (16 B) while `push_row` also pushed an
    // `Option<Window>` (24 B) — a 2.5x optimistic bound. This pins the window
    // slot as counted, and fails if the per-cell charge is reverted to 16 B.
    #[test]
    fn approx_bytes_counts_the_window_slot() {
        use std::mem::size_of;
        // The constants are claims about layout; check them against the layout.
        assert_eq!(size_of::<Option<u64>>(), VALUE_SLOT_BYTES);
        assert_eq!(size_of::<Option<i64>>(), VALUE_SLOT_BYTES);
        assert_eq!(size_of::<Option<Box<[u64]>>>(), VALUE_SLOT_BYTES);
        // `Window` is two `u64`s with no niche, so the option tag costs a word.
        assert_eq!(size_of::<Option<Window>>(), WINDOW_SLOT_BYTES);

        let md = cmeta("s");
        let mut b = TableBuilder::new("s".to_string());
        b.push_row(
            1_000,
            0,
            &[cell(
                "0",
                &md,
                Some(Window::new(900, 1_000)),
                CellValue::Counter(1),
            )],
        );

        // One value slot and one window slot were pushed, so both must be paid
        // for. The strict inequality is the part that catches a revert.
        let charged = b.approx_bytes();
        assert_eq!(
            charged,
            VALUE_SLOT_BYTES + WINDOW_SLOT_BYTES,
            "a scalar cell costs 40 B, not 16 B"
        );
        assert!(
            charged > VALUE_SLOT_BYTES,
            "the window slot must be accounted, not just the value slot"
        );
        let table = b.finish();
        assert_eq!(
            table.columns[0].windows.len(),
            1,
            "push_row pushed the window slot that was charged for"
        );
    }

    // Regression: the type-mismatch arm used to skip the value but still push
    // the window, desyncing every later row's window from its value. An agent
    // restart can remap a numeric id and flip a column's shape mid-recording,
    // and segmentation would bake the skew into immutable data.
    #[test]
    fn mismatch_arm_does_not_desync_values_and_windows() {
        let md = cmeta("s");
        let mut b = TableBuilder::new("s".to_string());
        let w = |n: u64| Some(Window::new(n, n + 100));

        b.push_row(1_000, 0, &[cell("0", &md, w(900), CellValue::Counter(1))]);
        // Same column name, now a gauge: shape mismatch against the established
        // counter column.
        b.push_row(2_000, 0, &[cell("0", &md, w(1_900), CellValue::Gauge(-5))]);
        b.push_row(3_000, 0, &[cell("0", &md, w(2_900), CellValue::Counter(3))]);

        let table = b.finish();
        assert_eq!(table.timestamps.len(), 3);
        for col in &table.columns {
            assert_eq!(
                col.windows.len(),
                TableBuilder::col_len(col),
                "column {} desynced: {} windows vs {} values",
                col.name,
                col.windows.len(),
                TableBuilder::col_len(col)
            );
        }
        // The mismatched row is a null, and the rows around it keep their own
        // windows (row 2's window is not shifted onto row 3).
        match &table.columns[0].values {
            RezValues::Counter(v) => assert_eq!(v, &vec![Some(1), None, Some(3)]),
            other => panic!("expected counter column, got {other:?}"),
        }
        assert_eq!(
            table.columns[0].windows,
            vec![
                Some(Window::new(900, 1_000)),
                None,
                Some(Window::new(2_900, 3_000))
            ]
        );
    }

    // The raw wall-clock reading rides along as a table-level sidecar column so
    // an NTP step locates to the exact tick; the query engine skips it.
    #[test]
    fn wall_offsets_roundtrip_through_parquet() {
        let md = cmeta("cpu_usage");
        let mut b = TableBuilder::new("cpu_usage".to_string());
        b.push_row(
            1_000,
            -123_456,
            &[cell("0", &md, None, CellValue::Counter(7))],
        );
        let table = b.finish();
        assert_eq!(table.wall_offsets, vec![-123_456]);

        let (schema, _) = table_to_batch(&table).unwrap();
        let field = schema
            .field_with_name(":wall_offset")
            .expect(":wall_offset field present");
        assert_eq!(field.data_type(), &arrow::datatypes::DataType::Int64);

        let bytes = write_table_parquet(&table).unwrap();
        let back = read_table_parquet("cpu_usage".to_string(), bytes).unwrap();
        assert_eq!(back.wall_offsets, vec![-123_456]);
        assert_eq!(back.timestamps, vec![1_000]);
    }
}

#[cfg(test)]
mod manifest_tests {
    use super::*;

    // A v1 manifest (single `file` per table, no `complete`/clock fields) must
    // still parse: v2 only adds fields, and `file` stays readable as a
    // one-element segment list.
    #[test]
    fn v1_manifest_json_still_parses() {
        let v1 = r#"{"version":1,"recordings":[{"dir":"rezolus","labels":{},
        "metadata":{},"tables":[{"sampler":"cpu_usage","file":"cpu_usage.parquet",
        "columns":["5"],"rows":3,"cadence_ns":1000000000}]}]}"#;
        let m: RezManifest = serde_json::from_str(v1).unwrap();
        let t = &m.recordings[0].tables[0];
        assert_eq!(t.segment_files(), vec!["cpu_usage.parquet"]);
        assert!(!m.recordings[0].complete); // absent -> false
    }

    #[test]
    fn v2_roundtrip_files_complete_clock() {
        let m = RezManifest {
            version: REZ_SCHEMA_VERSION,
            recordings: vec![RezRecording {
                dir: "rezolus".to_string(),
                labels: BTreeMap::new(),
                metadata: BTreeMap::new(),
                complete: true,
                clock_anchor_wall_ns: Some(1_700_000_000_000_000_000),
                clock_offsets: vec![(1_700_000_001_000_000_000, -2_500_000)],
                tables: vec![RezTableIndex {
                    sampler: "cpu_usage".to_string(),
                    file: None,
                    files: vec![
                        "cpu_usage/0.parquet".to_string(),
                        "cpu_usage/1.parquet".to_string(),
                    ],
                    columns: vec!["5".to_string()],
                    rows: 7,
                    cadence_ns: Some(1_000_000_000),
                }],
            }],
        };
        let json = serde_json::to_string(&m).unwrap();
        // A multi-segment table never serializes a null `file` (a v1 reader
        // would take `null` as a present-but-broken path rather than absent).
        assert!(!json.contains("\"file\":null"), "{json}");
        let back: RezManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
        assert_eq!(
            back.recordings[0].tables[0].segment_files(),
            vec!["cpu_usage/0.parquet", "cpu_usage/1.parquet"]
        );
    }

    #[test]
    fn manifest_json_round_trips() {
        let m = RezManifest {
            version: REZ_SCHEMA_VERSION,
            recordings: vec![RezRecording {
                dir: "rezolus".to_string(),
                labels: [
                    ("source".to_string(), "rezolus".to_string()),
                    ("host".to_string(), "node0".to_string()),
                ]
                .into_iter()
                .collect(),
                metadata: [("sampling_interval_ms".to_string(), "1000".to_string())]
                    .into_iter()
                    .collect(),
                complete: true,
                clock_anchor_wall_ns: None,
                clock_offsets: Vec::new(),
                tables: vec![RezTableIndex {
                    sampler: "cpu_usage".to_string(),
                    file: Some("cpu_usage.parquet".to_string()),
                    files: vec!["cpu_usage.parquet".to_string()],
                    columns: vec!["5".to_string()],
                    rows: 3,
                    cadence_ns: Some(1_000_000_000),
                }],
            }],
        };
        let bytes = serde_json::to_vec(&m).unwrap();
        let back: RezManifest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(m, back);
        assert_eq!(REZ_MANIFEST_NAME, "manifest.json");
    }
}

#[cfg(test)]
mod table_tests {
    use super::*;
    use crate::table::write_table_parquet;
    use crate::window::Window;
    use std::collections::HashMap;

    fn meta(metric: &str, mtype: &str) -> HashMap<String, String> {
        [
            ("metric".to_string(), metric.to_string()),
            ("sampler".to_string(), "s".to_string()),
            ("metric_type".to_string(), mtype.to_string()),
        ]
        .into_iter()
        .collect()
    }

    #[test]
    fn single_table_parquet_round_trips_values_and_windows() {
        let ts = vec![1_000u64, 2_000u64];
        let table = RezTable {
            sampler: "s".to_string(),
            timestamps: ts.clone(),
            wall_offsets: Vec::new(),
            columns: vec![
                RezColumn {
                    name: "0".to_string(),
                    metadata: meta("c", "counter"),
                    values: RezValues::Counter(vec![Some(10), Some(20)]),
                    windows: vec![
                        Some(Window::new(900, 1_000)),
                        Some(Window::new(1_900, 2_050)),
                    ],
                },
                RezColumn {
                    name: "1".to_string(),
                    metadata: meta("g", "gauge"),
                    values: RezValues::Gauge(vec![Some(-5), None]),
                    windows: vec![None, None],
                },
                RezColumn {
                    name: "2".to_string(),
                    metadata: {
                        let mut m = meta("h", "histogram");
                        m.insert("grouping_power".to_string(), "1".to_string());
                        m.insert("max_value_power".to_string(), "3".to_string());
                        m
                    },
                    values: RezValues::Histogram(vec![
                        Some(
                            histogram::Histogram::from_buckets(1, 3, vec![0, 1, 1, 0, 0, 0])
                                .unwrap(),
                        ),
                        None,
                    ]),
                    windows: vec![Some(Window::new(800, 1_000)), None],
                },
            ],
            table_window: None,
        };

        let bytes = write_table_parquet(&table).unwrap();
        let back = read_table_parquet("s".to_string(), bytes).unwrap();

        assert_eq!(back.timestamps, ts);
        assert_eq!(back.columns.len(), 3);
        // counter values + per-row windows preserved
        match &back.columns[0].values {
            RezValues::Counter(v) => assert_eq!(v, &vec![Some(10), Some(20)]),
            _ => panic!("expected counter"),
        }
        assert_eq!(
            back.columns[0].windows,
            vec![
                Some(Window::new(900, 1_000)),
                Some(Window::new(1_900, 2_050))
            ]
        );
        // gauge nulls + null windows preserved
        match &back.columns[1].values {
            RezValues::Gauge(v) => assert_eq!(v, &vec![Some(-5), None]),
            _ => panic!("expected gauge"),
        }
        assert_eq!(back.columns[1].windows, vec![None, None]);
        // histogram buckets preserved
        match &back.columns[2].values {
            RezValues::Histogram(v) => {
                assert_eq!(v[0].as_ref().unwrap().as_slice(), &[0, 1, 1, 0, 0, 0]);
                assert!(v[1].is_none());
            }
            _ => panic!("expected histogram"),
        }
    }
}

#[cfg(test)]
mod archive_tests {
    use super::*;
    use crate::table::write_table_parquet;
    use std::collections::HashMap;
    use std::path::PathBuf;

    /// A fixture written by the `.rez` writers at 0c39b2d (after 0.4.0;
    /// output kept at 0.4.0's shape); see
    /// `tests/rez_read.rs` for what each holds.
    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/rez")
            .join(name)
    }

    fn counter_table(sampler: &str, vals: Vec<Option<u64>>) -> RezTable {
        RezTable {
            sampler: sampler.to_string(),
            timestamps: (1..=vals.len() as u64).map(|t| t * 1_000).collect(),
            wall_offsets: Vec::new(),
            columns: vec![RezColumn {
                name: "0".to_string(),
                metadata: [
                    ("metric".to_string(), "c".to_string()),
                    ("metric_type".to_string(), "counter".to_string()),
                ]
                .into_iter()
                .collect::<HashMap<_, _>>(),
                windows: vec![None; vals.len()],
                values: RezValues::Counter(vals),
            }],
            table_window: None,
        }
    }

    fn tar_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (name, bytes) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_path(name).unwrap();
            h.set_size(bytes.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append(&h, *bytes).unwrap();
        }
        b.into_inner().unwrap()
    }

    #[test]
    fn read_archive_bytes_returns_manifest_and_per_table_bytes() {
        let (manifest, recordings) = read_archive_bytes(&fixture("v2.rez")).unwrap();
        assert_eq!(manifest.recordings.len(), 1);
        assert_eq!(recordings.len(), 1);
        assert_eq!(recordings[0].dir, "rezolus-web-01");
        let samplers: Vec<&str> = recordings[0]
            .tables
            .iter()
            .map(|(s, _)| s.as_str())
            .collect();
        assert_eq!(samplers, ["a", "b"]);
        for (_, segments) in &recordings[0].tables {
            assert_eq!(segments.len(), 1);
            let bytes = &segments[0];
            assert_eq!(&bytes[..4], b"PAR1");
            assert_eq!(&bytes[bytes.len() - 4..], b"PAR1");
        }
        // Written whole, so the recording is complete by construction.
        assert!(recordings[0].complete);
    }

    /// Two recordings with a table of the same name each resolve to their
    /// own bytes: the reader keys tables by `<dir>/<file>`.
    #[test]
    fn the_same_sampler_in_two_recordings_reads_as_two_tables() {
        let one = write_table_parquet(&counter_table("cpu_usage", vec![Some(1), Some(2)])).unwrap();
        let two =
            write_table_parquet(&counter_table("cpu_usage", vec![Some(10), Some(20)])).unwrap();
        let recording = |dir: &str, arm: &str| RezRecording {
            dir: dir.to_string(),
            labels: [("arm".to_string(), arm.to_string())].into_iter().collect(),
            metadata: BTreeMap::new(),
            complete: true,
            clock_anchor_wall_ns: None,
            clock_offsets: Vec::new(),
            tables: vec![RezTableIndex {
                sampler: "cpu_usage".to_string(),
                file: Some("cpu_usage.parquet".to_string()),
                files: vec!["cpu_usage.parquet".to_string()],
                columns: vec!["0".to_string()],
                rows: 2,
                cadence_ns: None,
            }],
        };
        let manifest = serde_json::to_vec(&RezManifest {
            version: REZ_SCHEMA_VERSION,
            recordings: vec![recording("redis", "redis"), recording("valkey", "valkey")],
        })
        .unwrap();
        let tar = tar_bytes(&[
            (REZ_MANIFEST_NAME, &manifest),
            ("redis/cpu_usage.parquet", &one),
            ("valkey/cpu_usage.parquet", &two),
        ]);

        let (manifest, recordings) = read_archive_reader(std::io::Cursor::new(tar)).unwrap();
        assert_eq!(manifest.recordings[1].labels["arm"], "valkey");
        let values = |i: usize| {
            let (sampler, segments) = recordings[i].tables[0].clone();
            let table = read_table_parquet(sampler, segments[0].clone()).unwrap();
            match table.columns[0].values.clone() {
                RezValues::Counter(v) => v,
                other => panic!("expected a counter, got {other:?}"),
            }
        };
        assert_eq!(values(0), vec![Some(1), Some(2)]);
        assert_eq!(values(1), vec![Some(10), Some(20)]);
    }

    #[test]
    fn is_rez_distinguishes_rez_from_bare_parquet() {
        assert!(is_rez_path(&fixture("v2.rez")).unwrap());

        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().join("plain.parquet");
        std::fs::write(
            &bare,
            write_table_parquet(&counter_table("cpu_usage", vec![Some(1)])).unwrap(),
        )
        .unwrap();
        assert!(!is_rez_path(&bare).unwrap());
    }

    #[test]
    fn detects_v3_sqlite_v2_tar_and_neither() {
        assert_eq!(
            detect_rez_format(&fixture("v3.rez")).unwrap(),
            RezFormat::V3Sqlite
        );
        assert_eq!(
            detect_rez_format(&fixture("v2.rez")).unwrap(),
            RezFormat::V2Tar
        );

        let dir = tempfile::tempdir().unwrap();
        // Neither: a bare parquet file.
        let pq = dir.path().join("x.parquet");
        std::fs::write(&pq, b"PAR1").unwrap();
        assert_eq!(detect_rez_format(&pq).unwrap(), RezFormat::NotRez);

        // Neither: a file too short to hold the SQLite magic. This must not panic
        // or error — it falls through to the tar sniff and comes back NotRez.
        let tiny = dir.path().join("tiny");
        std::fs::write(&tiny, b"hi").unwrap();
        assert_eq!(detect_rez_format(&tiny).unwrap(), RezFormat::NotRez);
    }

    /// `is_rez_path` is the tar sniff: a v3 file is not a tar, so it says
    /// false, and a caller that must accept both uses `detect_rez_format`.
    #[test]
    fn is_rez_path_is_true_for_a_tar_only() {
        assert!(is_rez_path(&fixture("v2.rez")).unwrap());
        assert!(!is_rez_path(&fixture("v3.rez")).unwrap());
    }
}

/// Truncation tolerance and manifest recovery. These build tars by hand
/// because the geometry of the cut is the thing under test: the `tar` crate
/// reports each failure mode differently (mid-data → a silently short entry,
/// mid-header → an error, block boundary → clean EOF).
#[cfg(test)]
mod recovery_tests {
    use super::*;
    use std::io::Cursor;

    const SEG_A: &[u8] = b"segment-a-parquet-bytes";
    const SEG_B: &[u8] = b"segment-b-parquet-bytes-which-are-longer";

    fn tar_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (name, bytes) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_path(name).unwrap();
            h.set_size(bytes.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append(&h, *bytes).unwrap();
        }
        b.into_inner().unwrap()
    }

    /// Offset just past `entries`, i.e. where the next entry's header starts
    /// (the tar minus its 1024-byte footer).
    fn tar_prefix_len(entries: &[(&str, &[u8])]) -> usize {
        tar_bytes(entries).len() - 1024
    }

    /// A manifest naming `files` (relative to the recording dir) as segments of
    /// the one `cpu_usage` table. `complete` marks a finalize manifest.
    fn manifest(files: &[&str], complete: bool) -> Vec<u8> {
        let tables = if files.is_empty() {
            Vec::new()
        } else {
            vec![RezTableIndex {
                sampler: "cpu_usage".to_string(),
                file: None,
                files: files.iter().map(|f| f.to_string()).collect(),
                columns: Vec::new(),
                rows: files.len() as u64,
                cadence_ns: None,
            }]
        };
        serde_json::to_vec(&RezManifest {
            version: REZ_SCHEMA_VERSION,
            recordings: vec![RezRecording {
                dir: "rezolus".to_string(),
                labels: BTreeMap::new(),
                metadata: BTreeMap::new(),
                complete,
                clock_anchor_wall_ns: Some(1_700_000_000_000_000_000),
                clock_offsets: Vec::new(),
                tables,
            }],
        })
        .unwrap()
    }

    /// The shape a streaming writer produces: an initial empty manifest, then
    /// alternating segments and checkpoint manifests, then the final one.
    fn checkpointed_entries() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        (
            manifest(&[], false),
            manifest(&["cpu_usage/0.parquet"], false),
            manifest(&["cpu_usage/0.parquet", "cpu_usage/1.parquet"], true),
        )
    }

    fn read(bytes: &[u8]) -> Result<(RezManifest, Vec<RecordingBytes>), RezError> {
        read_archive_reader(Cursor::new(bytes.to_vec()))
    }

    /// `read` expecting failure (`RecordingBytes` holds blobs, so it is
    /// deliberately not `Debug`/`unwrap_err`-able).
    fn read_err(bytes: &[u8]) -> String {
        match read(bytes) {
            Ok(_) => panic!("expected the archive to fail to open"),
            Err(e) => e.to_string(),
        }
    }

    /// Asserts the archive recovered to the checkpoint that knows only segment A.
    fn assert_recovered_to_first_segment(bytes: &[u8]) {
        let (manifest, recs) = read(bytes).expect("truncated archive recovers");
        assert!(
            !manifest.recordings[0].complete,
            "a checkpoint manifest is never complete"
        );
        assert_eq!(recs.len(), 1);
        assert!(!recs[0].complete);
        assert_eq!(recs[0].tables.len(), 1);
        assert_eq!(recs[0].tables[0].0, "cpu_usage");
        assert_eq!(recs[0].tables[0].1, vec![SEG_A.to_vec()]);
    }

    #[test]
    fn multi_segment_tables_resolve_in_order() {
        let (m0, m1, m2) = checkpointed_entries();
        let tar = tar_bytes(&[
            (REZ_MANIFEST_NAME, &m0),
            ("rezolus/cpu_usage/0.parquet", SEG_A),
            (REZ_MANIFEST_NAME, &m1),
            ("rezolus/cpu_usage/1.parquet", SEG_B),
            (REZ_MANIFEST_NAME, &m2),
        ]);
        let (manifest, recs) = read(&tar).unwrap();
        assert!(manifest.recordings[0].complete);
        assert_eq!(recs.len(), 1);
        assert!(recs[0].complete);
        assert_eq!(recs[0].tables.len(), 1);
        assert_eq!(recs[0].tables[0].0, "cpu_usage");
        // Segment order is manifest order, not tar order.
        assert_eq!(
            recs[0].tables[0].1,
            vec![SEG_A.to_vec(), SEG_B.to_vec()],
            "segments resolve in manifest order"
        );
    }

    /// A hand-built v1 manifest: `version: 1`, a single `file` per table, and
    /// no `complete` field at all (it did not exist yet).
    fn v1_manifest() -> Vec<u8> {
        br#"{
            "version": 1,
            "recordings": [{
                "dir": "rezolus",
                "labels": {},
                "metadata": {},
                "tables": [{
                    "sampler": "cpu_usage",
                    "file": "cpu_usage.parquet",
                    "columns": [],
                    "rows": 1,
                    "cadence_ns": null
                }]
            }]
        }"#
        .to_vec()
    }

    fn v1_archive() -> Vec<u8> {
        let m = v1_manifest();
        tar_bytes(&[
            (REZ_MANIFEST_NAME, &m),
            ("rezolus/cpu_usage.parquet", SEG_A),
        ])
    }

    /// v1 writers emitted the whole archive at once, so a v1 recording is
    /// complete by construction and the absent flag must not be interpreted.
    /// Normalizing at the read boundary — on the returned `RezManifest`, not
    /// just on `RecordingBytes` — is what keeps the tools honest: they copy
    /// `RezRecording.complete` verbatim into an output stamped `version: 2`.
    #[test]
    fn v1_manifest_reads_as_complete() {
        let (manifest, recs) = read(&v1_archive()).unwrap();
        assert_eq!(manifest.version, 1);
        assert!(
            manifest.recordings[0].complete,
            "a v1 recording is complete by construction"
        );
        assert!(recs[0].complete);
    }

    #[test]
    fn version_gate_rejects_newer() {
        let tar = tar_bytes(&[(REZ_MANIFEST_NAME, br#"{"version":3,"recordings":[]}"#)]);
        let err = read_err(&tar);
        assert!(err.contains('3') && err.contains('2'), "{err}");
    }

    // The subtle one, and the only geometry where the read-length check is what
    // decides the outcome: a manifest that *precedes* the data it names (how
    // the whole-archive tar writer laid an archive out) can reference a
    // segment the truncation ate. `tar` yields that entry `Ok` with a silently
    // short buffer, so without the length check M2 would "resolve" against a
    // corrupt stub instead of falling back to M1.
    #[test]
    fn truncated_mid_data_falls_back_to_previous_manifest() {
        let m1 = manifest(&["cpu_usage/0.parquet"], false);
        let m2 = manifest(&["cpu_usage/0.parquet", "cpu_usage/1.parquet"], true);
        let entries: Vec<(&str, &[u8])> = vec![
            (REZ_MANIFEST_NAME, &m1),
            ("rezolus/cpu_usage/0.parquet", SEG_A),
            (REZ_MANIFEST_NAME, &m2),
            ("rezolus/cpu_usage/1.parquet", SEG_B),
        ];
        let full = tar_bytes(&entries);
        let seg_b_data = tar_prefix_len(&entries[..3]) + 512;
        assert_recovered_to_first_segment(&full[..seg_b_data + SEG_B.len() - 2]);
    }

    #[test]
    fn truncated_mid_header_recovers() {
        let (m0, m1, m2) = checkpointed_entries();
        let entries: Vec<(&str, &[u8])> = vec![
            (REZ_MANIFEST_NAME, &m0),
            ("rezolus/cpu_usage/0.parquet", SEG_A),
            (REZ_MANIFEST_NAME, &m1),
            ("rezolus/cpu_usage/1.parquet", SEG_B),
            (REZ_MANIFEST_NAME, &m2),
        ];
        let full = tar_bytes(&entries);
        assert_recovered_to_first_segment(&full[..tar_prefix_len(&entries[..3]) + 100]);
    }

    #[test]
    fn truncated_mid_manifest_falls_back() {
        let (m0, m1, m2) = checkpointed_entries();
        let entries: Vec<(&str, &[u8])> = vec![
            (REZ_MANIFEST_NAME, &m0),
            ("rezolus/cpu_usage/0.parquet", SEG_A),
            (REZ_MANIFEST_NAME, &m1),
            ("rezolus/cpu_usage/1.parquet", SEG_B),
            (REZ_MANIFEST_NAME, &m2),
        ];
        let full = tar_bytes(&entries);
        let m2_data = tar_prefix_len(&entries[..4]) + 512;
        // What this pins is the geometry: segment B is fully present, but the
        // manifest naming it is not readable, so recovery must fall back to the
        // previous checkpoint's view rather than to segment B. It does *not*
        // isolate the read-length check — ablating that check leaves this test
        // passing, because a truncated JSON tail is independently unparseable
        // and the parse-failure branch catches it. Only
        // `truncated_mid_data_falls_back_to_previous_manifest` fails without it.
        assert_recovered_to_first_segment(&full[..m2_data + m2.len() - 3]);
    }

    // A full-length manifest entry whose JSON is garbage (the parse-failure
    // branch, as opposed to the short-read branch above).
    #[test]
    fn unparseable_tail_manifest_falls_back() {
        let (m0, m1, _) = checkpointed_entries();
        let tar = tar_bytes(&[
            (REZ_MANIFEST_NAME, &m0),
            ("rezolus/cpu_usage/0.parquet", SEG_A),
            (REZ_MANIFEST_NAME, &m1),
            (REZ_MANIFEST_NAME, b"{\"version\":2,\"recor"),
        ]);
        assert_recovered_to_first_segment(&tar);
    }

    #[test]
    fn block_boundary_truncation_is_clean_eof() {
        let (m0, m1, m2) = checkpointed_entries();
        let entries: Vec<(&str, &[u8])> = vec![
            (REZ_MANIFEST_NAME, &m0),
            ("rezolus/cpu_usage/0.parquet", SEG_A),
            (REZ_MANIFEST_NAME, &m1),
            ("rezolus/cpu_usage/1.parquet", SEG_B),
            (REZ_MANIFEST_NAME, &m2),
        ];
        let full = tar_bytes(&entries);
        // Cut exactly after a complete entry, with no footer: iteration ends
        // with no error at all, and the last complete manifest wins.
        assert_recovered_to_first_segment(&full[..tar_prefix_len(&entries[..3])]);
    }

    // `resolve` *moves* blobs out rather than cloning them, so two index
    // entries naming one file would leave the second table empty. Reject
    // instead — and, per the two-phase contract, without having touched
    // `blobs`, so an older manifest can still be tried against them.
    #[test]
    fn resolve_rejects_a_file_referenced_twice() {
        let table = |sampler: &str| RezTableIndex {
            sampler: sampler.to_string(),
            file: Some("cpu_usage.parquet".to_string()),
            files: Vec::new(),
            columns: Vec::new(),
            rows: 1,
            cadence_ns: None,
        };
        let manifest = RezManifest {
            version: REZ_SCHEMA_VERSION,
            recordings: vec![RezRecording {
                dir: "rezolus".to_string(),
                labels: BTreeMap::new(),
                metadata: BTreeMap::new(),
                complete: true,
                clock_anchor_wall_ns: None,
                clock_offsets: Vec::new(),
                tables: vec![table("cpu_usage"), table("scheduler")],
            }],
        };
        let mut blobs: HashMap<String, Vec<u8>> =
            [("rezolus/cpu_usage.parquet".to_string(), SEG_A.to_vec())]
                .into_iter()
                .collect();

        let err = match resolve(&manifest, &mut blobs) {
            Ok(_) => panic!("expected the duplicate reference to be rejected"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("referenced twice"), "{err}");
        assert!(err.contains("rezolus/cpu_usage.parquet"), "{err}");
        assert_eq!(
            blobs.get("rezolus/cpu_usage.parquet").map(Vec::as_slice),
            Some(SEG_A),
            "a failed resolve leaves the blobs intact"
        );
    }

    // Tolerance is for truncation, not for a manifest that lies: with no older
    // manifest to fall back to, an absent table file is still an error.
    #[test]
    fn single_manifest_missing_file_errors() {
        let m = manifest(&["cpu_usage/0.parquet"], true);
        let tar = tar_bytes(&[(REZ_MANIFEST_NAME, &m)]);
        let err = read_err(&tar);
        assert!(err.contains("rezolus/cpu_usage/0.parquet"), "{err}");
    }
}

#[cfg(test)]
mod label_tests {
    use super::*;

    #[test]
    fn host_from_systeminfo_extracts_hostname() {
        let json = r#"{"hostname":"node7","cpus":64}"#;
        assert_eq!(host_from_systeminfo(json), Some("node7".to_string()));
    }

    #[test]
    fn host_from_systeminfo_missing_or_invalid_is_none() {
        assert_eq!(host_from_systeminfo(r#"{"cpus":64}"#), None);
        assert_eq!(host_from_systeminfo("not json"), None);
        assert_eq!(host_from_systeminfo(r#"{"hostname":null}"#), None);
    }

    #[test]
    fn recording_dir_slug_sanitizes_source() {
        let labels: BTreeMap<String, String> = [("source".to_string(), "llm-perf".to_string())]
            .into_iter()
            .collect();
        assert_eq!(recording_dir_slug(&labels), "llm-perf");
    }

    #[test]
    fn recording_dir_slug_replaces_unsafe_chars_and_defaults() {
        let labels: BTreeMap<String, String> = [("source".to_string(), "a/b c".to_string())]
            .into_iter()
            .collect();
        assert_eq!(recording_dir_slug(&labels), "a-b-c");
        assert_eq!(recording_dir_slug(&BTreeMap::new()), "recording");
    }

    #[test]
    fn recording_dir_slug_qualifies_so_a_multi_host_archive_does_not_collide() {
        let slug = |pairs: &[(&str, &str)]| {
            recording_dir_slug(
                &pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            )
        };
        // The documented multi-host capture: both recordings are
        // `source=rezolus`, and they draw NO identical-labels warning because
        // the label sets genuinely differ on `host`. Without a qualifier both
        // displayed as "rezolus".
        assert_eq!(
            slug(&[("source", "rezolus"), ("host", "web-01")]),
            "rezolus-web-01"
        );
        assert_ne!(
            slug(&[("source", "rezolus"), ("host", "web-01")]),
            slug(&[("source", "rezolus"), ("host", "web-02")])
        );

        // `host` beats `arm`, because `--label` is global to a run: with
        // `--label arm=redis` across two endpoints, `arm` is the same on both
        // and only `host` tells them apart.
        assert_eq!(
            slug(&[("source", "rezolus"), ("host", "web-01"), ("arm", "redis")]),
            "rezolus-web-01"
        );
        // `arm` is the fallback for a `combine`-assembled archive, where the
        // arms were recorded separately and may carry no host.
        assert_eq!(
            slug(&[("source", "rezolus"), ("arm", "redis")]),
            "rezolus-redis"
        );

        // Sanitization runs on the CONCATENATED string, not just the source.
        assert_eq!(slug(&[("source", "a/b"), ("host", "c d")]), "a-b-c-d");

        // Genuinely indistinguishable labels still collide — that is the case
        // the recorder warns about at startup rather than papering over.
        assert_eq!(
            slug(&[("source", "rezolus"), ("host", "alpha")]),
            slug(&[("source", "rezolus"), ("host", "alpha")])
        );
    }
}

/// Regression coverage for #1067: one malformed histogram cell must not be
/// able to stall a recording.
#[cfg(test)]
mod malformed_histogram_tests {
    use super::{read_table_parquet, RezValues};
    use crate::builder::GroupTableBuilder;
    use crate::table::write_table_parquet;
    use metriken_model::{GroupSchema, MetricDesc};
    use std::collections::BTreeMap;

    /// A histogram cell's `(grouping_power, max_value_power, buckets)` comes
    /// off the WAL as msgpack and is not re-validated by
    /// `GroupSnapshot::validate`, which only checks arity and the schema hash.
    /// Rebuilding it through `Histogram::from_buckets` can therefore fail.
    ///
    /// That failure used to propagate out of `GroupTableBuilder::push_row` and
    /// so out of `materialize_wal_tail` — and because the row stayed in the
    /// WAL, every later attempt failed identically. One bad cell stalled the
    /// recorder, or a hindsight buffer, permanently.
    ///
    /// The blast radius was wider than "sealing" by the time this was fixed:
    /// `materialize_wal_tail` is also how the reader materializes a live WAL
    /// tail, and how the `.rez` rewrite tools (`combine`, `filter`, `upgrade`,
    /// hindsight's dump) carried one across. All of them inherited it.
    ///
    /// `push_row` is infallible now, so the wedge is impossible by type rather
    /// than by promise. Three things are asserted, and the middle one is what
    /// makes the fix worth more than a `let _ =`: the seal succeeds, the OTHER
    /// member of the same group keeps every value, and the bad member reads as
    /// absent rather than as a fabricated histogram.
    #[test]
    fn a_malformed_histogram_cell_does_not_wedge_the_seal() {
        let desc = |name: &str| MetricDesc {
            name: name.to_string(),
            metadata: BTreeMap::from([("metric".to_string(), name.to_string())]),
        };
        let schema = GroupSchema {
            counters: vec![desc("good_counter")],
            gauges: Vec::new(),
            histograms: vec![desc("bad_histogram")],
            gauge_histograms: Vec::new(),
        };

        let mut builder = GroupTableBuilder::new("cpu_usage/sweep".to_string());
        for row in 0..3u64 {
            builder.push_row(
                1_000 + row,
                0,
                None,
                &(&schema).into(),
                &[Some(row + 1)],
                &[],
                // grouping_power >= max_value_power is rejected by
                // `from_buckets` — the shape a corrupt or truncated WAL cell
                // takes.
                &[Some((9, 3, vec![0, 1, 2, 3]))],
            );
        }

        let table = builder.finish();
        let bytes = write_table_parquet(&table).expect("a malformed cell must not fail the encode");
        let back = read_table_parquet("cpu_usage/sweep".to_string(), bytes)
            .expect("and the sealed segment must read back");

        let counter = back
            .columns
            .iter()
            .find(|c| c.name == "good_counter")
            .expect("the healthy member's column survived");
        match &counter.values {
            RezValues::Counter(v) => assert_eq!(
                v.iter().flatten().count(),
                3,
                "every counter row must survive a sibling's malformed histogram"
            ),
            other => panic!("expected a counter column, got {other:?}"),
        }

        if let Some(hist) = back.columns.iter().find(|c| c.name == "bad_histogram") {
            if let RezValues::Histogram(v) = &hist.values {
                assert!(
                    v.iter().all(|h| h.is_none()),
                    "a cell that failed to rebuild must read as absent, never fabricated"
                );
            }
        }
    }
}

/// A group slot that is relabeled or reused must not write into the column its
/// previous occupant created — see #1205.
#[cfg(test)]
mod slot_relabel_tests {
    use super::{read_table_parquet, RezValues};
    use crate::builder::GroupTableBuilder;
    use crate::schema::{GroupSchema, MetricDesc};
    use crate::table::write_table_parquet;

    /// One gauge in slot 0, named as a group slot is: `{metric_id}x{slot}`,
    /// unchanged by a relabel.
    fn schema(mount: &str, device: &str) -> GroupSchema {
        GroupSchema {
            gauges: vec![MetricDesc {
                name: "123x0".into(),
                metadata: [
                    ("metric".to_string(), "filesystem_total".to_string()),
                    ("mount".to_string(), mount.to_string()),
                    ("device".to_string(), device.to_string()),
                ]
                .into_iter()
                .collect(),
            }],
            ..Default::default()
        }
    }

    fn gauges(col: &super::RezColumn) -> Vec<Option<i64>> {
        match &col.values {
            RezValues::Gauge(v) => v.clone(),
            other => panic!("expected gauges, got {other:?}"),
        }
    }

    /// The bug, as reported: two identities, one column, the first one's
    /// labels, and a value that belongs to the second filed under the first.
    #[test]
    fn a_relabeled_slot_does_not_inherit_the_previous_labels() {
        let (old, new) = (schema("/old", "8:1"), schema("/new", "8:17"));
        assert_ne!(old.hash(), new.hash(), "a relabel changes the schema hash");

        let mut builder = GroupTableBuilder::new("filesystem/filesystem_sweep".into());
        builder.push_row(1, 0, None, &old, &[], &[Some(100)], &[]);
        builder.push_row(2, 0, None, &new, &[], &[Some(200)], &[]);
        let table = builder.finish();

        assert_eq!(
            table.columns.len(),
            2,
            "each identity needs its own column; got {:?}",
            table.columns.iter().map(|c| &c.name).collect::<Vec<_>>()
        );

        let first = &table.columns[0];
        assert_eq!(first.metadata["mount"], "/old");
        assert_eq!(
            gauges(first),
            vec![Some(100), None],
            "the old identity has no reading once the slot was relabeled"
        );

        let second = &table.columns[1];
        assert_eq!(second.metadata["mount"], "/new");
        assert_eq!(second.metadata["device"], "8:17");
        assert_eq!(
            gauges(second),
            vec![None, Some(200)],
            "the new identity is absent for rows before it existed, not zero"
        );
        assert_ne!(
            first.name, second.name,
            "parquet field names must be unique within a schema"
        );
    }

    /// The ordinary case must not regress: an unchanged descriptor keeps
    /// writing to one column, or every group table would explode into a column
    /// per row.
    #[test]
    fn an_unchanged_slot_keeps_one_column() {
        let s = schema("/data", "8:1");
        let mut builder = GroupTableBuilder::new("filesystem/filesystem_sweep".into());
        for (i, v) in [100i64, 200, 300].into_iter().enumerate() {
            builder.push_row(i as u64 + 1, 0, None, &s, &[], &[Some(v)], &[]);
        }
        let table = builder.finish();
        assert_eq!(table.columns.len(), 1);
        assert_eq!(
            gauges(&table.columns[0]),
            vec![Some(100), Some(200), Some(300)]
        );
    }

    /// A slot that returns to a previous identity gets a THIRD column rather
    /// than resuming the first. Resuming would fuse two disjoint occupancies
    /// into one series across a gap where the slot meant something else —
    /// which is the same misattribution in a subtler form.
    #[test]
    fn a_slot_returning_to_an_earlier_identity_gets_a_new_column() {
        let (a, b) = (schema("/a", "8:1"), schema("/b", "8:2"));
        let mut builder = GroupTableBuilder::new("filesystem/filesystem_sweep".into());
        builder.push_row(1, 0, None, &a, &[], &[Some(1)], &[]);
        builder.push_row(2, 0, None, &b, &[], &[Some(2)], &[]);
        builder.push_row(3, 0, None, &a, &[], &[Some(3)], &[]);
        let table = builder.finish();
        assert_eq!(table.columns.len(), 3);
        assert_eq!(gauges(&table.columns[0]), vec![Some(1), None, None]);
        assert_eq!(gauges(&table.columns[2]), vec![None, None, Some(3)]);
    }

    /// The generation suffix must survive a parquet round trip, and the two
    /// generations must come back as two columns with their own labels.
    #[test]
    fn both_generations_survive_a_parquet_round_trip() {
        let (old, new) = (schema("/old", "8:1"), schema("/new", "8:17"));
        let mut builder = GroupTableBuilder::new("filesystem/filesystem_sweep".into());
        builder.push_row(1, 0, None, &old, &[], &[Some(100)], &[]);
        builder.push_row(2, 0, None, &new, &[], &[Some(200)], &[]);

        let bytes = write_table_parquet(&builder.finish()).unwrap();
        let table = read_table_parquet("filesystem/filesystem_sweep".to_string(), bytes).unwrap();

        assert_eq!(table.columns.len(), 2);
        let mounts: Vec<&str> = table
            .columns
            .iter()
            .map(|c| c.metadata["mount"].as_str())
            .collect();
        assert!(
            mounts.contains(&"/old") && mounts.contains(&"/new"),
            "both identities must survive the round trip: {mounts:?}"
        );
    }
}
