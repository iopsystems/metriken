//! The process's own registry as a local source (feature `registry`).

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use metriken_exposition::group_builder::{GroupBuilder, MemberNames, Router, Stamp as PassStamp};
use metriken_model::Snapshot;

use crate::{LocalSource, SourceIdentity, Stamp};

/// A [`LocalSource`] over the metriken registry of this process: each pass
/// is [`GroupBuilder::snapshot`], stamped on the process's timeline
/// (`metriken::epoch`), and its identity carries the process's clock anchor
/// and producer epoch.
pub struct RegistrySource<R: Router, N: MemberNames> {
    builder: GroupBuilder<R, N>,
    labels: BTreeMap<String, String>,
    metadata: BTreeMap<String, String>,
}

impl<R: Router, N: MemberNames> RegistrySource<R, N> {
    /// A source recorded with `labels` (a `source` names it) and `metadata`.
    pub fn new(
        builder: GroupBuilder<R, N>,
        labels: BTreeMap<String, String>,
        metadata: BTreeMap<String, String>,
    ) -> Self {
        Self {
            builder,
            labels,
            metadata,
        }
    }
}

impl<R, N> LocalSource for RegistrySource<R, N>
where
    R: Router + Send + 'static,
    N: MemberNames + Send + 'static,
{
    fn identity(&self) -> SourceIdentity {
        let anchor = metriken::epoch::clock_anchor_wall_ns();
        SourceIdentity {
            labels: self.labels.clone(),
            metadata: self.metadata.clone(),
            clock_anchor_wall_ns: u64::try_from(anchor).unwrap_or_default(),
            epoch: Some(metriken::epoch::producer_epoch().to_string()),
        }
    }

    fn pass(&mut self) -> Option<(Snapshot, Stamp)> {
        let stamp = PassStamp::now();
        let snapshot = self
            .builder
            .snapshot(stamp, Duration::ZERO, Vec::new(), HashMap::new());
        Some((
            Snapshot::V3(snapshot),
            Stamp {
                ts: u64::try_from(stamp.ts).ok()?,
                wall_offset: stamp.wall_offset,
            },
        ))
    }
}
