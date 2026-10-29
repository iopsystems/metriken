//! An open-time label overlay: labels a caller adds to, changes on or
//! removes from an archive's sources for as long as it holds the reader,
//! never written to the archive.
//!
//! An overlay is a list of entries, each a selector (a source's uuid, or
//! matchers on its labels) and the labels to set on the sources it selects.
//! It is applied where the catalog reports each source's labels
//! ([`LabelOverlay::catalog`]), so everything the reader derives from them
//! (read groups, `__recording__`, query routing, listings) sees the overlaid
//! labels. Open an archive with one through
//! [`ArchiveReader::open_read_groups_overlaid`](crate::ArchiveReader::open_read_groups_overlaid),
//! or wrap a [`Catalog`] for [`ArchiveReader::from_catalog`](crate::ArchiveReader::from_catalog).

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::catalog::{Catalog, SegmentIndex, SegmentMeta, Source, Span, WalRow};
use crate::labels::Labels;

/// Which sources an [`OverlayEntry`] applies to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceSelector {
    /// The source with this uuid, and its copies, which keep it.
    Uuid(String),
    /// The sources whose labels, as the archive stores them, `Labels::matches`
    /// this filter (one matcher per label: `v`, `~v` for `=~`, `!v` for `!=`
    /// and `!~`).
    Matching(Labels),
}

/// One entry of a [`LabelOverlay`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayEntry {
    pub selector: SourceSelector,
    /// The labels to set on each source the selector selects; a value of
    /// `""` removes the label.
    pub labels: BTreeMap<String, String>,
}

/// Labels set on an archive's sources at open, above its stored labels.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LabelOverlay {
    entries: Vec<OverlayEntry>,
}

impl LabelOverlay {
    /// An overlay of `entries`, applied in order: where two entries select
    /// one source and set one key, the later wins. Every selector is decided
    /// on the labels the archive stores, not on what an earlier entry set.
    /// An error for an entry setting `__recording__`, which the reader
    /// assigns.
    pub fn new(entries: Vec<OverlayEntry>) -> Result<Self, String> {
        for entry in &entries {
            if entry.labels.contains_key(crate::labeled::RECORDING_LABEL) {
                return Err(format!(
                    "an overlay entry sets {}, which the reader assigns",
                    crate::labeled::RECORDING_LABEL
                ));
            }
        }
        Ok(Self { entries })
    }

    /// `labels`, of the source with `uuid`, with the overlay applied.
    pub fn apply(
        &self,
        labels: &BTreeMap<String, String>,
        uuid: Option<&str>,
    ) -> BTreeMap<String, String> {
        let stored = Labels::from(labels.clone());
        let mut out = labels.clone();
        for entry in &self.entries {
            let selected = match &entry.selector {
                SourceSelector::Uuid(u) => uuid == Some(u.as_str()),
                SourceSelector::Matching(filter) => stored.matches(filter),
            };
            if !selected {
                continue;
            }
            for (k, v) in &entry.labels {
                match v.is_empty() {
                    true => out.remove(k),
                    false => out.insert(k.clone(), v.clone()),
                };
            }
        }
        out
    }

    /// `inner`, reporting each source's labels with the overlay applied.
    pub fn catalog(self: &Arc<Self>, inner: Box<dyn Catalog>) -> Box<dyn Catalog> {
        Box::new(OverlaidCatalog {
            inner,
            overlay: Arc::clone(self),
        })
    }
}

/// A catalog whose sources' labels carry an overlay.
struct OverlaidCatalog {
    inner: Box<dyn Catalog>,
    overlay: Arc<LabelOverlay>,
}

impl Catalog for OverlaidCatalog {
    fn sources(&self) -> Result<Vec<Source>, String> {
        Ok(self
            .sources_with_uuids()?
            .into_iter()
            .map(|(source, _)| source)
            .collect())
    }

    fn sources_with_uuids(&self) -> Result<Vec<(Source, Option<String>)>, String> {
        Ok(self
            .inner
            .sources_with_uuids()?
            .into_iter()
            .map(|(mut source, uuid)| {
                source.labels = self.overlay.apply(&source.labels, uuid.as_deref());
                (source, uuid)
            })
            .collect())
    }

    fn tables(&self, source_id: i64) -> Result<Vec<String>, String> {
        self.inner.tables(source_id)
    }

    fn segment_meta(&self, source_id: i64, table: &str) -> Result<Vec<(u64, SegmentMeta)>, String> {
        self.inner.segment_meta(source_id, table)
    }

    fn segment_bytes(
        &self,
        source_id: i64,
        table: &str,
        seq: u64,
    ) -> Result<Option<Vec<u8>>, String> {
        self.inner.segment_bytes(source_id, table, seq)
    }

    fn segment_bytes_matching(
        &self,
        source_id: i64,
        table: &str,
        seq: u64,
        meta: &SegmentMeta,
    ) -> Result<Option<Vec<u8>>, String> {
        self.inner
            .segment_bytes_matching(source_id, table, seq, meta)
    }

    fn live_wal(&self, source_id: i64, table: &str) -> Result<Vec<WalRow>, String> {
        self.inner.live_wal(source_id, table)
    }

    fn read_snapshot(&self, f: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        self.inner.read_snapshot(f)
    }

    fn segment_indexes(&self, source_id: i64, table: &str) -> Result<Vec<SegmentIndex>, String> {
        self.inner.segment_indexes(source_id, table)
    }

    fn segment_span(&self, source_id: i64, table: &str) -> Result<(u64, Span), String> {
        self.inner.segment_span(source_id, table)
    }

    fn live_wal_span(&self, source_id: i64, table: &str) -> Result<Span, String> {
        self.inner.live_wal_span(source_id, table)
    }

    fn caller_row_streams(&self, source_id: i64) -> Result<Vec<String>, String> {
        self.inner.caller_row_streams(source_id)
    }

    fn caller_rows(
        &self,
        source_id: i64,
        stream: &str,
        from: u64,
        to: u64,
    ) -> Result<Vec<(u64, Vec<u8>)>, String> {
        self.inner.caller_rows(source_id, stream, from, to)
    }

    fn last_caller_row_at_or_before(
        &self,
        source_id: i64,
        stream: &str,
        upto: u64,
        pred: &mut dyn FnMut(&[u8]) -> bool,
    ) -> Result<Option<u64>, String> {
        self.inner
            .last_caller_row_at_or_before(source_id, stream, upto, pred)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn entry(selector: SourceSelector, labels: &[(&str, &str)]) -> OverlayEntry {
        OverlayEntry {
            selector,
            labels: map(labels),
        }
    }

    /// Entries apply in order on the stored labels: a selector does not see
    /// what an earlier entry set, the later entry wins a key, and `""`
    /// removes one.
    #[test]
    fn entries_apply_in_order_on_the_stored_labels() {
        let overlay = LabelOverlay::new(vec![
            entry(
                SourceSelector::Matching(Labels::from(map(&[("source", "a")]))),
                &[("source", "z"), ("arm", "x"), ("host", "")],
            ),
            entry(
                SourceSelector::Matching(Labels::from(map(&[("source", "z")]))),
                &[("never", "1")],
            ),
            entry(SourceSelector::Uuid("u1".into()), &[("arm", "y")]),
        ])
        .unwrap();
        let stored = map(&[("source", "a"), ("host", "h")]);
        assert_eq!(
            overlay.apply(&stored, Some("u1")),
            map(&[("source", "z"), ("arm", "y")])
        );
        assert_eq!(
            overlay.apply(&stored, None),
            map(&[("source", "z"), ("arm", "x")])
        );
        let other = map(&[("source", "b")]);
        assert_eq!(overlay.apply(&other, Some("u2")), other);
    }

    #[test]
    fn an_entry_setting_recording_is_refused() {
        let refused = LabelOverlay::new(vec![entry(
            SourceSelector::Uuid("u".into()),
            &[(crate::labeled::RECORDING_LABEL, "1")],
        )]);
        assert!(refused.is_err());
    }
}
