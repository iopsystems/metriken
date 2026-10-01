//! Walking the metriken registry into acquisition groups: a [`SnapshotV3`].
//!
//! Moved from rezolus's agent (`create_v3` and its skeleton cache), with
//! everything rezolus-specific behind a caller-supplied [`Router`]. Phase 5b
//! of `docs/journal/2026-09-29-members-that-come-and-go.md`.
//!
//! # What the router decides and what the builder does
//!
//! For each registered metric the [`Router`] says which group it belongs to
//! (a [`GroupId`]) and how its members are chosen (a [`Membership`]), or that
//! it is not exposed at all. For each group it says where the group's
//! acquisition window comes from ([`Acquisition`]). It can add keys to every
//! member's metadata ([`Router::annotate`]).
//!
//! The builder does the rest, identically for every router:
//!
//! - each member's [`MetricDesc`]: `metric` (the registry name), the metric's
//!   static metadata, the router's keys, `id` and the slot's metadata for a
//!   group member, `grouping_power`/`max_value_power` for a histogram, with
//!   [`GROUP_METADATA_KEY`] removed;
//! - member names, `{metric_id}` and `{metric_id}x{idx}` by default
//!   ([`Positional`]), where `metric_id` is the metric's position in
//!   `metriken::metrics()`;
//! - the schema and its hash ([`GroupSchema::hash`]);
//! - the skeleton cache, which reuses a group's schema and hash while its
//!   membership is unchanged (see [`GroupBuilder`]).
//!
//! # Membership
//!
//! - [`Membership::Present`]: membership follows values. A counter group entry
//!   reading `0` or unwritten, a gauge group entry unwritten, or a histogram
//!   that has not loaded is not a member. This is the V2 rule, kept for groups
//!   nothing has declared, so a producer's undeclared metrics do not publish a
//!   member for every slot of every backing array.
//! - [`Membership::All`], [`Membership::Prefix`], [`Membership::Set`]:
//!   membership follows registration. Every listed slot is a member, and one
//!   with no reading is `None` rather than absent. A declared histogram that
//!   has not loaded is `None` too.
//! - [`Membership::Slots`]: the slots that carry metadata, walked from the
//!   group's metadata store (O(live slots), not O(capacity)). This is what a
//!   slot space managed by [`metriken::group::SlotIdentity`] is, and what a
//!   family ([`metriken::CounterFamily`], [`metriken::GaugeFamily`]) is.
//!
//! # The long form
//!
//! [`GroupBuilder::build_stream`] emits a group whose metrics are all counter
//! groups or gauge groups as a [`LongGroupSnapshot`]: the metrics as columns,
//! and one [`LongMember`] per member slot, carrying the slot's labels and a
//! key that names its occupant. Its columns change when the
//! group's metrics do, and a slot's labels when its occupant does, so a
//! change of occupant does not rebuild the group's schema.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

/// The builder's own maps, keyed by group and looked up once or twice per
/// registry entry per build. foldhash rather than SipHash: the keys are the
/// builder's own strings, not attacker-chosen.
type FastMap<K, V> = HashMap<K, V, foldhash::fast::RandomState>;
use std::time::{Duration, SystemTime};

use metriken::{MetricEntry, Value, Window};

use crate::{GroupSchema, GroupSnapshot, MetricDesc, SnapshotV3};

/// The static metadata key that names the group a metric belongs to.
///
/// Removed from every member's metadata. On a declared group it repeats the
/// group's own name in every member, which for a group of thousands of slots
/// is thousands of copies of one key. On a metric the router sent elsewhere
/// (a name with no matching group) it would be a label naming a group the
/// member is not in.
pub const GROUP_METADATA_KEY: &str = "acq_group";

/// A group's identity. Its wire name is `"{namespace}/{name}"`, the form
/// [`GroupSnapshot::name`] documents and archives split at the first `/`.
///
/// Two borrowed parts rather than one string so routing a metric allocates
/// nothing: the wire name is formatted once per group per build, not once per
/// registry entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GroupId<'a> {
    pub namespace: &'a str,
    pub name: &'a str,
}

impl<'a> GroupId<'a> {
    pub const fn new(namespace: &'a str, name: &'a str) -> Self {
        Self { namespace, name }
    }

    /// `"{namespace}/{name}"`.
    pub fn wire_name(&self) -> String {
        format!("{}/{}", self.namespace, self.name)
    }
}

/// Which members of a metric are in its group. See the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Membership<'a> {
    /// Membership follows values (the V2 sentinel rule).
    Present,
    /// Every slot of the backing array.
    All,
    /// The first `n` slots, clamped to the backing array.
    Prefix(usize),
    /// Exactly these slots, sorted ascending, clamped to the backing array.
    Set(&'a [usize]),
    /// The slots that carry metadata.
    Slots,
}

impl Membership<'_> {
    /// Whether membership follows registration rather than values.
    fn registered(&self) -> bool {
        !matches!(self, Membership::Present)
    }
}

/// Where a metric goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route<'a> {
    pub group: GroupId<'a>,
    pub membership: Membership<'a>,
}

/// Where a group's acquisition window comes from, decided when the builder
/// first touches the group in a build.
pub enum Acquisition<G> {
    /// No window.
    Windowless,
    /// A window some writer stamps, read now, before any of the group's
    /// values. The builder reads it again through [`Router::window`] after
    /// the group's values and emits the union of the two reads (see
    /// [`resolve_walk_window`]).
    Stamped(Option<Window>),
    /// The builder's own read of the group is the acquisition: `G` is opened
    /// now, told when each group metric's values have been read
    /// ([`ReadGuard::mark_end`]), and finished when the group is emitted. A
    /// group that produces no values drops its guard unfinished.
    Reader(G),
}

/// The bracket around a read the builder itself performs. See
/// [`Acquisition::Reader`].
pub trait ReadGuard {
    /// The values of one group metric have just been read. Called after each
    /// counter-group or gauge-group metric's members, so the window's end is
    /// when the group's own values were read and not when the rest of the
    /// registry walk finished.
    fn mark_end(&mut self) {}

    /// Publish the bracket and return the group's window.
    fn finish(self) -> Option<Window>;
}

/// The guard of a router that never returns [`Acquisition::Reader`].
pub enum NoGuard {}

impl ReadGuard for NoGuard {
    fn finish(self) -> Option<Window> {
        match self {}
    }
}

/// The producer-specific half of building groups. See the module docs.
///
/// The builder calls [`route`](Router::route) for every metric in two walks
/// per build, so it must answer the same way both times for a registry that
/// did not change in between.
pub trait Router {
    type Guard: ReadGuard;

    /// The group `metric` belongs to and how its members are chosen, or
    /// `None` to leave it out of the snapshot.
    fn route<'a>(&'a self, metric: &'a MetricEntry) -> Option<Route<'a>>;

    /// Called once per build when a group is first touched, before any of
    /// its values are read.
    fn acquire(&self, group: GroupId<'_>) -> Acquisition<Self::Guard> {
        let _ = group;
        Acquisition::Windowless
    }

    /// The second read of a [`Stamped`](Acquisition::Stamped) group's window,
    /// after its values.
    fn window(&self, group: GroupId<'_>) -> Option<Window> {
        let _ = group;
        None
    }

    /// Add keys to the metadata every member of `metric` carries. Called only
    /// when a schema is built, after the metric's static metadata and before
    /// [`GROUP_METADATA_KEY`] is removed.
    fn annotate(&self, metric: &MetricEntry, metadata: &mut BTreeMap<String, String>) {
        let _ = (metric, metadata);
    }
}

/// How members are named. A name must be unique across every group of a
/// snapshot ([`MetricDesc::name`]).
pub trait MemberNames {
    /// A counter, gauge or histogram metric: the member is the metric.
    fn metric(&self, metric_id: usize, metric: &MetricEntry) -> String;
    /// Slot `idx` of a counter group or gauge group.
    fn member(&self, metric_id: usize, metric: &MetricEntry, idx: usize) -> String;
}

/// `{metric_id}` and `{metric_id}x{idx}`, by position in the registry. What
/// existing archives carry.
///
/// A metric registered before another shifts the position of every metric
/// after it, which changes those members' names and every schema hash they
/// are in.
#[derive(Clone, Copy, Debug, Default)]
pub struct Positional;

impl MemberNames for Positional {
    fn metric(&self, metric_id: usize, _: &MetricEntry) -> String {
        format!("{metric_id}")
    }

    fn member(&self, metric_id: usize, _: &MetricEntry, idx: usize) -> String {
        format!("{metric_id}x{idx}")
    }
}

/// `{name}` and `{name}#{idx}`, by the metric's registry name. Stable across
/// registration order. Unique only when no two exposed metrics share a name,
/// which the caller must ensure: two members with one name drop one reading
/// downstream.
#[derive(Clone, Copy, Debug, Default)]
pub struct ByName;

impl MemberNames for ByName {
    fn metric(&self, _: usize, metric: &MetricEntry) -> String {
        metric.name().to_string()
    }

    fn member(&self, _: usize, metric: &MetricEntry, idx: usize) -> String {
        format!("{}#{idx}", metric.name())
    }
}

/// A group not backed by registry metrics, appended by the caller: pushed
/// metrics, for instance. Always built fresh (never a cache hit).
///
/// If a routed group has the same namespace and name, these members are
/// appended after its registry members and the routed group's window is
/// kept; otherwise `window` is the group's window.
pub struct ExtraGroup {
    pub namespace: String,
    pub name: String,
    pub window: Option<Window>,
    pub counters: Vec<(MetricDesc, Option<u64>)>,
    pub gauges: Vec<(MetricDesc, Option<i64>)>,
    pub histograms: Vec<(MetricDesc, Option<histogram::Histogram>)>,
}

/// When a build read its values: a timestamp on the process's timeline
/// ([`metriken::epoch`]) and the wall clock's disagreement with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    /// `anchor + monotonic elapsed`: wall-clock magnitude, never decreasing.
    pub ts: i64,
    /// Wall clock minus `ts` at the reading. `ts + wall_offset` is the wall
    /// clock.
    pub wall_offset: i64,
}

impl Stamp {
    /// This moment, from [`metriken::epoch::anchored_now`].
    pub fn now() -> Self {
        let (ts, wall_offset) = metriken::epoch::anchored_now();
        Self { ts, wall_offset }
    }

    /// The wall clock this stamp names: `ts + wall_offset`, clamped to the
    /// Unix epoch.
    pub fn systemtime(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_nanos((self.ts + self.wall_offset).max(0) as u64)
    }
}

/// Builds [`SnapshotV3`] groups from the metriken registry, reusing each
/// group's schema while its membership is unchanged.
///
/// # The skeleton cache
///
/// A group's schema (every member's name and metadata) and its hash are the
/// expensive part of a build: a `MetricDesc`, a formatted name and a metadata
/// map per member. Each build first folds every group's membership into a
/// cheap identity (no allocation per member): for each metric, its registry
/// position; for a group metric, its [`metadata_version`] and the indices of
/// its members. A group whose identity matches the cached one reuses the
/// cached `Arc<GroupSchema>` and hash and only reads values; any other group
/// builds its schema again.
///
/// The identity covers the per-slot metadata version, not only member
/// names, because metadata changes in place at a stable index (a slot
/// relabelled, an occupant replaced) while `{metric_id}x{idx}` stays the
/// same. A cache that compared names only would keep serving the old
/// occupant's labels under an unchanged `schema_hash`, and a receiver
/// caching schemas by `(name, schema_hash)` would bind new values to them
/// indefinitely.
///
/// Values and windows are not part of the identity: they change every
/// tick.
///
/// **The identity stored on a rebuild is folded from the rebuilding walk
/// itself**, not from the first pass. For a [`Membership::Present`] group
/// the two passes read values separately, and a value can cross the
/// membership boundary in between. Storing the first pass's identity would
/// bind this schema to a membership the walk did not see; if membership later
/// returned to what the first pass saw, every later build would hit and ship
/// this schema with values collected under a different membership, and would
/// not recover. Storing the walk's own identity keeps "the stored identity
/// describes the stored schema" true.
///
/// **On a hit, the value counts are checked against the cached schema.** The
/// same race can make the first pass call a hit while the walk collects a
/// different number of members. A mismatch evicts the entry and leaves the
/// group out of that build; the next build rebuilds it. One case passes the
/// check: an equal-count swap inside that window (one member drops below the
/// sentinel as another rises above it) binds that build's values to the
/// previous schema's labels. Closing it would mean re-folding the identity
/// on the hit path, which costs the per-member metadata read the hit path
/// exists to avoid. Registered membership has no such window: it does not
/// depend on values, so both passes agree.
///
/// Entries are never evicted except by that check. The key space is the set
/// of groups a build can produce, which does not grow with members.
///
/// [`metadata_version`]: metriken::CounterGroupMetric::metadata_version
pub struct GroupBuilder<R: Router, N: MemberNames = Positional> {
    router: R,
    names: N,
    cache: FastMap<String, GroupSkeleton>,
    /// Per long group of [`build_stream`](Self::build_stream): its columns
    /// and its slots' occupants.
    long: FastMap<String, LongState>,
    rebuilds: u64,
}

struct GroupSkeleton {
    /// The membership fingerprint of the build that made `schema`.
    identity: (u64, u64),
    schema: Arc<GroupSchema>,
    hash: (u64, u64),
}

impl<R: Router> GroupBuilder<R, Positional> {
    /// A builder with an empty cache and [`Positional`] names.
    pub fn new(router: R) -> Self {
        Self {
            router,
            names: Positional,
            cache: FastMap::default(),
            long: FastMap::default(),
            rebuilds: 0,
        }
    }
}

impl<R: Router, N: MemberNames> GroupBuilder<R, N> {
    /// The same builder naming members with `names`. The caches are emptied,
    /// since cached schemas carry the old names.
    pub fn with_names<M: MemberNames>(self, names: M) -> GroupBuilder<R, M> {
        GroupBuilder {
            router: self.router,
            names,
            cache: FastMap::default(),
            long: FastMap::default(),
            rebuilds: 0,
        }
    }

    pub fn router(&self) -> &R {
        &self.router
    }

    /// How many times a group's schema has been built (and hashed) since
    /// this builder was created.
    pub fn rebuilds(&self) -> u64 {
        self.rebuilds
    }

    /// A [`SnapshotV3`] of every routed group plus `extra`.
    ///
    /// `metadata` is copied into the snapshot, then these keys are set:
    ///
    /// - `producer_epoch` ([`metriken::epoch::producer_epoch`]): what lets a
    ///   consumer notice the process restarted between two snapshots, when
    ///   every counter restarted from zero together and the values alone
    ///   cannot say so;
    /// - `clock_anchor_wall_ns` ([`metriken::epoch::clock_anchor_wall_ns`]):
    ///   the timeline `ts` is on, carried on every snapshot because a
    ///   snapshot is all some consumers read, and an anchor fetched
    ///   separately could belong to another run;
    /// - `ts` and `wall_offset` from `stamp`: when the values were read,
    ///   which is not when the snapshot is sent or received.
    ///
    /// `systemtime` is `stamp`'s wall clock, so `ts + wall_offset ==
    /// systemtime` exactly.
    pub fn snapshot(
        &mut self,
        stamp: Stamp,
        duration: Duration,
        extra: Vec<ExtraGroup>,
        metadata: HashMap<String, String>,
    ) -> SnapshotV3 {
        let groups = self.build_groups(extra);
        let mut metadata = metadata;
        metadata.insert(
            "producer_epoch".to_string(),
            metriken::epoch::producer_epoch().to_string(),
        );
        metadata.insert(
            "clock_anchor_wall_ns".to_string(),
            metriken::epoch::clock_anchor_wall_ns().to_string(),
        );
        metadata.insert("ts".to_string(), stamp.ts.to_string());
        metadata.insert("wall_offset".to_string(), stamp.wall_offset.to_string());
        SnapshotV3 {
            systemtime: stamp.systemtime(),
            duration,
            metadata,
            groups,
        }
    }

    /// Every routed group plus `extra`, sorted by name.
    ///
    /// A group nothing routed to is absent, not empty, and so is a group
    /// whose only metrics have a value kind the builder does not expose
    /// (`Value::HistogramGroup`, `Value::Other`).
    pub fn build_groups(&mut self, extra: Vec<ExtraGroup>) -> Vec<GroupSnapshot> {
        // One registry guard for both passes, so both see the same dynamic
        // metrics. Registering or dropping a dynamic metric waits for it.
        let metrics = metriken::metrics();
        let Self {
            router,
            names,
            cache,
            rebuilds,
            ..
        } = self;
        build_wide(
            &*router,
            &*names,
            cache,
            rebuilds,
            &metrics,
            extra,
            &HashSet::new(),
        )
    }
}

/// The groups of [`GroupBuilder::build_groups`] except those in `skip`, from
/// one registry guard.
fn build_wide<R: Router, N: MemberNames>(
    router: &R,
    names: &N,
    cache: &mut FastMap<String, GroupSkeleton>,
    rebuilds: &mut u64,
    metrics: &metriken::Metrics,
    extra: Vec<ExtraGroup>,
    skip: &HashSet<GroupId<'_>>,
) -> Vec<GroupSnapshot> {
    let mut extra_ids: Vec<(String, String, Option<Window>)> = Vec::with_capacity(extra.len());
    let mut extra_members = Vec::with_capacity(extra.len());
    for g in extra {
        extra_ids.push((g.namespace, g.name, g.window));
        extra_members.push((g.counters, g.gauges, g.histograms));
    }
    let extra_keys: HashSet<GroupId<'_>> = extra_ids
        .iter()
        .map(|(ns, name, _)| GroupId::new(ns, name))
        .collect();

    let decisions = fold_group_identities(cache, router, metrics, &extra_keys, skip);

    let mut groups: FastMap<GroupId<'_>, Group<R::Guard>> = FastMap::default();
    // Reused by every `Slots` walk, cleared per metric: one buffer for
    // the whole build rather than one per group.
    let mut idx_scratch: Vec<usize> = Vec::new();

    for (metric_id, metric) in metrics.iter().enumerate() {
        let Some(value) = metric.value() else {
            continue;
        };
        let Some(route) = router.route(metric) else {
            continue;
        };
        if skip.contains(&route.group) {
            continue;
        }
        let registered = route.membership.registered();

        let group = match groups.entry(route.group) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => {
                // First touch of this group in this build. A stamped
                // window is read here, before any of the group's values:
                // a window can only lag its values, never lead them,
                // which is the safe direction. Reading it only after the
                // walk could pair a window from a writer's next cycle
                // with values read before it, claiming the data is newer
                // than it is. A reader guard opens here and is finished
                // at emit.
                let window = match router.acquire(*e.key()) {
                    Acquisition::Windowless => WindowState::Windowless,
                    Acquisition::Stamped(first) => WindowState::Stamped(first),
                    Acquisition::Reader(guard) => WindowState::Reader(guard),
                };
                let decision = decisions.get(e.key()).copied().unwrap_or_default();
                let mut group = Group::new(window, decision.needs_schema);

                // A hit: size this build's value vectors from the cached
                // schema (its sizes carried by the decision, so the cache
                // is not looked up again) and pushing values never
                // reallocates. At most three allocations per hit group,
                // none per member.
                if let Some((counters, gauges, histograms)) = decision.sizes {
                    group.counter_values = Vec::with_capacity(counters);
                    group.gauge_values = Vec::with_capacity(gauges);
                    group.histogram_values = Vec::with_capacity(histograms);
                }
                e.insert(group)
            }
        };

        if group.needs_schema {
            // MISS: build the descriptors. The member walks below must
            // choose members exactly as `fold_group_identities` does, or
            // the two passes disagree on membership.
            let mut metadata: BTreeMap<String, String> =
                [("metric".to_string(), metric.name().to_string())].into();
            for (k, v) in metric.metadata().iter() {
                metadata.insert(k.to_string(), v.to_string());
            }
            router.annotate(metric, &mut metadata);
            metadata.remove(GROUP_METADATA_KEY);

            let metric_id_u64 = metric_id as u64;

            match value {
                Value::Counter(v) => {
                    group.walk_identity.counters =
                        identity_fold(group.walk_identity.counters, &metric_id_u64.to_le_bytes());
                    group.counter_descs.push(MetricDesc {
                        name: names.metric(metric_id, metric),
                        metadata,
                    });
                    group.counter_values.push(Some(v));
                }
                Value::Gauge(v) => {
                    group.walk_identity.gauges =
                        identity_fold(group.walk_identity.gauges, &metric_id_u64.to_le_bytes());
                    group.gauge_descs.push(MetricDesc {
                        name: names.metric(metric_id, metric),
                        metadata,
                    });
                    group.gauge_values.push(Some(v));
                }
                Value::CounterGroup(g) => {
                    // The version first, then the members: see
                    // `fold_group_version`.
                    group.walk_identity.counters = fold_group_version(
                        group.walk_identity.counters,
                        metric_id_u64,
                        g.metadata_version(),
                    );
                    let members =
                        walk_members(route.membership, &mut idx_scratch, g.entries(), &mut |f| {
                            g.for_each_metadata(f)
                        });
                    for idx in members {
                        // The value and the metadata are two reads with no
                        // lock in common. A slot whose occupant changes
                        // between them pairs one occupant's value with the
                        // other's labels for this build. A producer that
                        // relabels a slot should do it in one
                        // `set_metadata` call, and between builds.
                        let v = g.counter_value(idx);
                        if !registered && !matches!(v, Some(v) if v != 0) {
                            continue;
                        }
                        // Fold what THIS walk observed, in the same order
                        // as the first pass, so a later build can
                        // reproduce it on a real hit. Membership only:
                        // the labels are covered by the version above.
                        group.walk_identity.counters = identity_fold(
                            group.walk_identity.counters,
                            &(idx as u64).to_le_bytes(),
                        );
                        // The metadata before the name: allocating them
                        // in this order measured faster on a rebuild (see
                        // `member_metadata`).
                        let member_md =
                            member_metadata(&metadata, idx, &mut |f| g.with_metadata(idx, f));
                        group.counter_descs.push(MetricDesc {
                            name: names.member(metric_id, metric, idx),
                            metadata: member_md,
                        });
                        group.counter_values.push(v);
                    }
                    group.mark_end();
                }
                Value::GaugeGroup(g) => {
                    group.walk_identity.gauges = fold_group_version(
                        group.walk_identity.gauges,
                        metric_id_u64,
                        g.metadata_version(),
                    );
                    let members =
                        walk_members(route.membership, &mut idx_scratch, g.entries(), &mut |f| {
                            g.for_each_metadata(f)
                        });
                    for idx in members {
                        // `gauge_value` maps the group's unwritten
                        // sentinel to `None`, so `None` is the whole
                        // value-derived test for a gauge.
                        let v = g.gauge_value(idx);
                        if !registered && v.is_none() {
                            continue;
                        }
                        group.walk_identity.gauges =
                            identity_fold(group.walk_identity.gauges, &(idx as u64).to_le_bytes());
                        // The metadata before the name: allocating them
                        // in this order measured faster on a rebuild (see
                        // `member_metadata`).
                        let member_md =
                            member_metadata(&metadata, idx, &mut |f| g.with_metadata(idx, f));
                        group.gauge_descs.push(MetricDesc {
                            name: names.member(metric_id, metric, idx),
                            metadata: member_md,
                        });
                        group.gauge_values.push(v);
                    }
                    group.mark_end();
                }
                Value::Histogram(h) => {
                    // `config()` needs no loaded value.
                    let mut metadata = metadata;
                    metadata.insert(
                        "grouping_power".to_string(),
                        h.config().grouping_power().to_string(),
                    );
                    metadata.insert(
                        "max_value_power".to_string(),
                        h.config().max_value_power().to_string(),
                    );
                    let hv = h.load();
                    // Registered: the metric is the member, and `None`
                    // means no reading yet. Omitting it would make
                    // membership follow values and change the schema on
                    // exactly the event (a histogram not yet loaded)
                    // registered membership exists to ignore.
                    if registered || hv.is_some() {
                        group.walk_identity.histograms = identity_fold(
                            group.walk_identity.histograms,
                            &metric_id_u64.to_le_bytes(),
                        );
                        group.histogram_descs.push(MetricDesc {
                            name: names.metric(metric_id, metric),
                            metadata,
                        });
                        group.histogram_values.push(hv);
                    }
                }
                _ => {}
            }
        } else {
            // HIT: identity unchanged since the cached build. Read values
            // in the same order and under the same membership rules as
            // the arms above, and build no descriptor.
            match value {
                Value::Counter(v) => group.counter_values.push(Some(v)),
                Value::Gauge(v) => group.gauge_values.push(Some(v)),
                Value::CounterGroup(g) => {
                    let members =
                        walk_members(route.membership, &mut idx_scratch, g.entries(), &mut |f| {
                            g.for_each_metadata(f)
                        });
                    for idx in members {
                        let v = g.counter_value(idx);
                        if !registered && !matches!(v, Some(v) if v != 0) {
                            continue;
                        }
                        group.counter_values.push(v);
                    }
                    group.mark_end();
                }
                Value::GaugeGroup(g) => {
                    let members =
                        walk_members(route.membership, &mut idx_scratch, g.entries(), &mut |f| {
                            g.for_each_metadata(f)
                        });
                    for idx in members {
                        let v = g.gauge_value(idx);
                        if !registered && v.is_none() {
                            continue;
                        }
                        group.gauge_values.push(v);
                    }
                    group.mark_end();
                }
                Value::Histogram(h) => {
                    let hv = h.load();
                    if registered || hv.is_some() {
                        group.histogram_values.push(hv);
                    }
                }
                _ => {}
            }
        }
    }

    // Caller-supplied groups: always built, appended after any registry
    // members of the same group.
    for ((namespace, name, window), (counters, gauges, histograms)) in
        extra_ids.iter().zip(extra_members)
    {
        let group = groups
            .entry(GroupId::new(namespace, name))
            .or_insert_with(|| Group::new(WindowState::Fixed(*window), true));
        debug_assert!(
            group.needs_schema,
            "group `{namespace}/{name}` has extra members but was not marked for a rebuild",
        );
        for (desc, v) in counters {
            group.counter_descs.push(desc);
            group.counter_values.push(v);
        }
        for (desc, v) in gauges {
            group.gauge_descs.push(desc);
            group.gauge_values.push(v);
        }
        for (desc, v) in histograms {
            group.histogram_descs.push(desc);
            group.histogram_values.push(v);
        }
    }

    let mut snapshots: Vec<GroupSnapshot> = Vec::with_capacity(groups.len());

    for (key, group) in groups {
        // A metric routes (and so creates its group) before its value kind
        // is matched, so a group whose only metrics are of a kind not
        // exposed here has nothing in it. Left out rather than emitted
        // empty. Checked on values, not descriptors, which are always
        // empty on a hit. A reader guard is dropped unfinished: nothing
        // was read, so there is nothing to publish.
        if group.counter_values.is_empty()
            && group.gauge_values.is_empty()
            && group.histogram_values.is_empty()
        {
            continue;
        }

        let group_name = key.wire_name();

        let window = match group.window {
            WindowState::Windowless => None,
            WindowState::Fixed(w) => w,
            WindowState::Stamped(first) => resolve_walk_window(first, router.window(key)),
            // The width was set by the last `mark_end`; finishing here
            // only decides when the window is published.
            WindowState::Reader(guard) => guard.finish(),
        };

        let (schema, hash) = if group.needs_schema {
            let schema = GroupSchema {
                counters: group.counter_descs,
                gauges: group.gauge_descs,
                histograms: group.histogram_descs,
            };
            let hash = schema.hash();
            let schema = Arc::new(schema);
            // The walk's own identity, not the first pass's: see the
            // type's docs.
            cache.insert(
                group_name.clone(),
                GroupSkeleton {
                    identity: group.walk_identity.finish(),
                    schema: schema.clone(),
                    hash,
                },
            );
            *rebuilds += 1;
            (schema, hash)
        } else {
            match cache.get(&group_name) {
                Some(cached)
                    if cached.schema.counters.len() == group.counter_values.len()
                        && cached.schema.gauges.len() == group.gauge_values.len()
                        && cached.schema.histograms.len() == group.histogram_values.len() =>
                {
                    (cached.schema.clone(), cached.hash)
                }
                // The first pass called a hit and this walk collected a
                // different membership: evict and leave the group out of
                // this build. A reader-guarded group has already
                // published its window above; its membership is
                // registered, so both passes agree and this is not
                // reached for one.
                Some(_) => {
                    cache.remove(&group_name);
                    continue;
                }
                // Not reached: a hit implies a cache entry.
                None => continue,
            }
        };

        snapshots.push(GroupSnapshot {
            name: group_name,
            schema_hash: hash,
            schema: Some(schema),
            window,
            counters: group.counter_values,
            gauges: group.gauge_values,
            histograms: group.histogram_values,
        });
    }

    snapshots.sort_by(|a, b| a.name.cmp(&b.name));
    snapshots
}

impl<R: Router, N: MemberNames> GroupBuilder<R, N> {
    /// Every routed group plus `extra`, sorted by name, with each slotted
    /// group in the long form.
    ///
    /// A group is long when every metric routed to it is a counter group or
    /// a gauge group and `extra` does not name it. Each member slot is an
    /// occupant ([`LongMember`]) holding one value per metric: every walked
    /// slot under registered membership, a slot with a value under
    /// [`Membership::Present`]. Any
    /// other group is a [`GroupSnapshot`], built as
    /// [`build_groups`](Self::build_groups) builds it.
    ///
    /// A long group's columns ([`LongGroupSnapshot::columns`]) are built
    /// when its set of metrics changes. An occupant's labels are built when
    /// its slot gets a new occupant, or, in a group with a
    /// [`Membership::Slots`] metric, when a slot returns after a build in
    /// which it carried no slot metadata. A change of occupant costs that slot's labels,
    /// not the group's schema.
    pub fn build_stream(&mut self, extra: Vec<ExtraGroup>) -> Vec<StreamGroup> {
        let metrics = metriken::metrics();
        let extra_keys: HashSet<(String, String)> = extra
            .iter()
            .map(|g| (g.namespace.clone(), g.name.clone()))
            .collect();
        let Self {
            router,
            names,
            cache,
            long: states,
            rebuilds,
        } = self;
        let router: &R = router;
        let long_keys = long_groups(router, &metrics, &extra_keys);
        let wide = build_wide(
            router, &*names, cache, rebuilds, &metrics, extra, &long_keys,
        );
        let long = build_long(router, &*names, states, rebuilds, &metrics, &long_keys);
        let mut groups: Vec<StreamGroup> = wide
            .into_iter()
            .map(StreamGroup::Wide)
            .chain(long.into_iter().map(StreamGroup::Long))
            .collect();
        groups.sort_by(|a, b| a.name().cmp(b.name()));
        groups
    }
}

fn build_long<R: Router, N: MemberNames>(
    router: &R,
    names: &N,
    states: &mut FastMap<String, LongState>,
    rebuilds: &mut u64,
    metrics: &metriken::Metrics,
    long_keys: &HashSet<GroupId<'_>>,
) -> Vec<LongGroupSnapshot> {
    if long_keys.is_empty() {
        return Vec::new();
    }

    let mut groups: FastMap<GroupId<'_>, LongAcc<'_, R::Guard>> = FastMap::default();
    let mut idx_scratch: Vec<usize> = Vec::new();

    for (metric_id, metric) in metrics.iter().enumerate() {
        let Some(value) = metric.value() else {
            continue;
        };
        let Some(route) = router.route(metric) else {
            continue;
        };
        if !long_keys.contains(&route.group) {
            continue;
        }
        let registered = route.membership.registered();
        let slot_membership = matches!(route.membership, Membership::Slots);
        let acc = groups.entry(route.group).or_insert_with_key(|key| {
            let name = key.wire_name();
            let state = states.remove(&name).unwrap_or_default();
            LongAcc {
                window: match router.acquire(*key) {
                    Acquisition::Windowless => WindowState::Windowless,
                    Acquisition::Stamped(first) => WindowState::Stamped(first),
                    Acquisition::Reader(guard) => WindowState::Reader(guard),
                },
                name,
                state,
                identity: IDENTITY_FNV_OFFSET,
                counter_metrics: Vec::new(),
                gauge_metrics: Vec::new(),
                versions: Vec::new(),
                slots: Vec::new(),
                slot_membership: false,
                position: FastMap::default(),
                occupants: Vec::new(),
            }
        });

        acc.slot_membership |= slot_membership;

        match value {
            Value::CounterGroup(g) => {
                let column = acc.counter_metrics.len();
                acc.counter_metrics.push((metric_id, metric));
                acc.identity = identity_fold(acc.identity, b"c");
                acc.identity = identity_fold(acc.identity, &(metric_id as u64).to_le_bytes());
                // Read before the members, as `fold_group_version`
                // explains: a change landing mid-walk shows next build.
                let version = g.metadata_version();
                let unchanged = acc.state.versions.get(&metric_id) == Some(&version);
                acc.versions.push((metric_id, version));
                let members =
                    walk_members(route.membership, &mut idx_scratch, g.entries(), &mut |f| {
                        g.for_each_metadata(f)
                    });
                for idx in members {
                    let v = g.counter_value(idx);
                    if !registered && !matches!(v, Some(v) if v != 0) {
                        continue;
                    }
                    let pos = acc.occupant(idx, unchanged, &mut |f| g.with_metadata(idx, f));
                    let counters = &mut acc.occupants[pos].counters;
                    if counters.len() <= column {
                        counters.resize(column + 1, None);
                    }
                    counters[column] = v;
                }
                acc.mark_end();
            }
            Value::GaugeGroup(g) => {
                let column = acc.gauge_metrics.len();
                acc.gauge_metrics.push((metric_id, metric));
                acc.identity = identity_fold(acc.identity, b"g");
                acc.identity = identity_fold(acc.identity, &(metric_id as u64).to_le_bytes());
                let version = g.metadata_version();
                let unchanged = acc.state.versions.get(&metric_id) == Some(&version);
                acc.versions.push((metric_id, version));
                let members =
                    walk_members(route.membership, &mut idx_scratch, g.entries(), &mut |f| {
                        g.for_each_metadata(f)
                    });
                for idx in members {
                    let v = g.gauge_value(idx);
                    if !registered && v.is_none() {
                        continue;
                    }
                    let pos = acc.occupant(idx, unchanged, &mut |f| g.with_metadata(idx, f));
                    let gauges = &mut acc.occupants[pos].gauges;
                    if gauges.len() <= column {
                        gauges.resize(column + 1, None);
                    }
                    gauges[column] = v;
                }
                acc.mark_end();
            }
            // `long_groups` admits only the two kinds above.
            _ => {}
        }
    }

    let mut out = Vec::with_capacity(groups.len());
    for (key, mut acc) in groups {
        let identity = ((acc.identity >> 64) as u64, acc.identity as u64);
        if acc.state.identity != Some(identity) {
            let columns = Arc::new(GroupSchema {
                counters: acc
                    .counter_metrics
                    .iter()
                    .map(|(id, m)| column_desc(router, names, *id, m))
                    .collect(),
                gauges: acc
                    .gauge_metrics
                    .iter()
                    .map(|(id, m)| column_desc(router, names, *id, m))
                    .collect(),
                histograms: Vec::new(),
            });
            acc.state.hash = columns.hash();
            acc.state.columns = columns;
            acc.state.identity = Some(identity);
            *rebuilds += 1;
        }
        acc.state.versions = acc.versions.into_iter().collect();
        // A slot of a group with a slot-metadata metric that was not walked
        // this build is forgotten: its next occupant has a new uid, and keeping every slot
        // ever seen would grow with every PID. Any other group keeps it, so
        // a slot that reads nothing for a build (a counter at zero under
        // value-derived membership) keeps its key when it returns.
        if acc.slot_membership {
            let position = &acc.position;
            acc.state.slots.retain(|idx, _| position.contains_key(idx));
        }

        if acc.occupants.is_empty() {
            // Nothing read: the window is not published, as for a wide
            // group with no values.
            states.insert(acc.name, acc.state);
            continue;
        }
        let window = match acc.window {
            WindowState::Windowless => None,
            WindowState::Fixed(w) => w,
            WindowState::Stamped(first) => resolve_walk_window(first, router.window(key)),
            WindowState::Reader(guard) => guard.finish(),
        };
        let widths = (acc.counter_metrics.len(), acc.gauge_metrics.len());
        let mut order: Vec<usize> = (0..acc.occupants.len()).collect();
        order.sort_unstable_by_key(|&i| acc.slots[i]);
        let mut occupants: Vec<Option<LongMember>> = acc.occupants.into_iter().map(Some).collect();
        let occupants: Vec<LongMember> = order
            .into_iter()
            .map(|i| {
                let mut o = occupants[i].take().expect("each position is taken once");
                o.counters.resize(widths.0, None);
                o.gauges.resize(widths.1, None);
                o
            })
            .collect();
        out.push(LongGroupSnapshot {
            name: acc.name.clone(),
            window,
            columns_hash: acc.state.hash,
            columns: Arc::clone(&acc.state.columns),
            occupants,
        });
        states.insert(acc.name, acc.state);
    }
    out
}

/// The groups [`GroupBuilder::build_stream`] writes long: every metric routed
/// to them is a counter group or a gauge group, and `extra` does not name
/// them.
fn long_groups<'a, R: Router>(
    router: &'a R,
    metrics: &'a metriken::Metrics,
    extra: &HashSet<(String, String)>,
) -> HashSet<GroupId<'a>> {
    let mut eligible: FastMap<GroupId<'a>, bool> = FastMap::default();
    for metric in metrics.iter() {
        let Some(value) = metric.value() else {
            continue;
        };
        let Some(route) = router.route(metric) else {
            continue;
        };
        let slotted = matches!(value, Value::CounterGroup(_) | Value::GaugeGroup(_));
        let ok = eligible.entry(route.group).or_insert(true);
        *ok &= slotted;
    }
    eligible
        .into_iter()
        .filter(|(key, ok)| {
            *ok && !extra.contains(&(key.namespace.to_string(), key.name.to_string()))
        })
        .map(|(key, _)| key)
        .collect()
}

/// A long group's column for one group metric: the metric's metadata, as a
/// wide member carries it, without `id` or the slot's labels.
fn column_desc<R: Router, N: MemberNames>(
    router: &R,
    names: &N,
    metric_id: usize,
    metric: &MetricEntry,
) -> MetricDesc {
    let mut metadata: BTreeMap<String, String> =
        [("metric".to_string(), metric.name().to_string())].into();
    for (k, v) in metric.metadata().iter() {
        metadata.insert(k.to_string(), v.to_string());
    }
    router.annotate(metric, &mut metadata);
    metadata.remove(GROUP_METADATA_KEY);
    MetricDesc {
        name: names.metric(metric_id, metric),
        metadata,
    }
}

/// One group of [`GroupBuilder::build_stream`].
#[derive(Clone, Debug)]
pub enum StreamGroup {
    Wide(GroupSnapshot),
    Long(LongGroupSnapshot),
}

impl StreamGroup {
    /// The group's wire name.
    pub fn name(&self) -> &str {
        match self {
            StreamGroup::Wide(g) => &g.name,
            StreamGroup::Long(g) => &g.name,
        }
    }
}

/// A group of counter groups and gauge groups in the long form: one entry
/// per occupant present, with the group's metrics as columns.
#[derive(Clone, Debug)]
pub struct LongGroupSnapshot {
    /// `"{namespace}/{name}"`, as [`GroupSnapshot::name`].
    pub name: String,
    pub window: Option<Window>,
    /// [`GroupSchema::hash`] of `columns`.
    pub columns_hash: (u64, u64),
    /// One descriptor per counter-group and gauge-group metric, in registry
    /// order: the metric's metadata, with no `id` and no slot labels. It has
    /// no histograms.
    pub columns: Arc<GroupSchema>,
    /// The member slots' occupants, in slot order.
    pub occupants: Vec<LongMember>,
}

/// One occupant of a [`LongGroupSnapshot`] and its values.
#[derive(Clone, Debug, PartialEq)]
pub struct LongMember {
    /// Names one occupant within its group for the life of the builder; an
    /// occupant can receive more than one key. Keys are assigned from 0 in
    /// the order occupants first appear, and a slot gets a new one when its
    /// occupant changes: a new [`UID_LABEL`], new labels on a slot without
    /// one, or, in a group with a [`Membership::Slots`] metric, a return
    /// after a build in which the slot carried no slot metadata. Small
    /// numbers keep a long row short on the wire.
    ///
    /// [`UID_LABEL`]: metriken::group::UID_LABEL
    pub key: u64,
    /// `id` (the slot) and the slot's metadata: the labels a wide member
    /// carries beyond its metric's. Taken from the first of the group's
    /// metrics, in registry order, that walks the slot this build, or the
    /// labels cached from the last build when that metric's metadata
    /// version is unchanged; a group whose metrics carry different metadata
    /// for one slot shows only one metric's.
    pub labels: Arc<BTreeMap<String, String>>,
    /// One per counter column, `None` where the metric has no value.
    pub counters: Vec<Option<u64>>,
    /// One per gauge column.
    pub gauges: Vec<Option<i64>>,
}

/// What [`GroupBuilder::build_stream`] keeps per long group between builds.
#[derive(Default)]
struct LongState {
    /// The metrics `columns` was built from.
    identity: Option<(u64, u64)>,
    columns: Arc<GroupSchema>,
    hash: (u64, u64),
    /// Each group metric's metadata version at the last build, by registry
    /// position.
    versions: FastMap<usize, u64>,
    /// The occupant each slot held at the last build.
    slots: FastMap<usize, SlotOccupant>,
    /// The key the group's next new occupant gets.
    next_key: u64,
}

#[derive(Clone)]
struct SlotOccupant {
    key: u64,
    labels: Arc<BTreeMap<String, String>>,
}

/// One long group during a build.
struct LongAcc<'m, G> {
    name: String,
    window: WindowState<G>,
    state: LongState,
    /// Folded from each metric's kind and registry position.
    identity: u128,
    counter_metrics: Vec<(usize, &'m MetricEntry)>,
    gauge_metrics: Vec<(usize, &'m MetricEntry)>,
    /// This build's metadata version of each group metric.
    versions: Vec<(usize, u64)>,
    /// Per position in `occupants`, its slot.
    slots: Vec<usize>,
    /// Whether any of the group's metrics takes its members from slot
    /// metadata ([`Membership::Slots`]).
    slot_membership: bool,
    position: FastMap<usize, usize>,
    occupants: Vec<LongMember>,
}

impl<G: ReadGuard> LongAcc<'_, G> {
    fn mark_end(&mut self) {
        if let WindowState::Reader(guard) = &mut self.window {
            guard.mark_end();
        }
    }

    /// The position of slot `idx`'s occupant in this build, added on its
    /// first value. Its labels are those of the last build when this metric's
    /// metadata is `unchanged`, or when the slot's metadata still names the
    /// same occupant; otherwise they are built from the metadata.
    fn occupant(
        &mut self,
        idx: usize,
        unchanged: bool,
        with_metadata: &mut WithMetadata<'_>,
    ) -> usize {
        if let Some(&pos) = self.position.get(&idx) {
            return pos;
        }
        let LongState {
            slots, next_key, ..
        } = &mut self.state;
        let cached = slots.get(&idx);
        let occupant = match cached {
            Some(c) if unchanged => c.clone(),
            _ => {
                let mut resolved = None;
                with_metadata(&mut |m| resolved = Some(slot_occupant(idx, m, cached, next_key)));
                resolved.unwrap_or_else(|| slot_occupant(idx, None, cached, next_key))
            }
        };
        self.state.slots.insert(idx, occupant.clone());
        let pos = self.occupants.len();
        self.position.insert(idx, pos);
        self.slots.push(idx);
        // Sized from the last build's columns, so a group whose metrics did
        // not change fills each occupant without reallocating.
        self.occupants.push(LongMember {
            key: occupant.key,
            labels: occupant.labels,
            counters: Vec::with_capacity(self.state.columns.counters.len()),
            gauges: Vec::with_capacity(self.state.columns.gauges.len()),
        });
        pos
    }
}

/// Slot `idx`'s occupant from its metadata `m`: `cached` when `m` describes
/// the same occupant, otherwise one built from `m` with the key `next_key`,
/// which is then advanced.
fn slot_occupant(
    idx: usize,
    m: Option<&HashMap<String, String>>,
    cached: Option<&SlotOccupant>,
    next_key: &mut u64,
) -> SlotOccupant {
    // A uid names one assignment, so equal uids are the same occupant.
    let uid = m.and_then(|m| m.get(metriken::group::UID_LABEL));
    if let (Some(uid), Some(c)) = (uid, cached) {
        if c.labels.get(metriken::group::UID_LABEL) == Some(uid) {
            return c.clone();
        }
    }
    let id = idx.to_string();
    let same = uid.is_none()
        && cached.is_some_and(|c| {
            let extra = m.map_or(0, |m| usize::from(!m.contains_key("id")));
            c.labels.get("id") == Some(&id)
                && c.labels.len() == m.map_or(0, HashMap::len) + extra
                && m.is_none_or(|m| {
                    m.iter()
                        .all(|(k, v)| k == "id" || c.labels.get(k) == Some(v))
                })
        });
    if let (true, Some(c)) = (same, cached) {
        return c.clone();
    }
    let mut labels: BTreeMap<String, String> = BTreeMap::new();
    labels.insert("id".to_string(), id);
    if let Some(m) = m {
        for (k, v) in m {
            labels.insert(k.clone(), v.clone());
        }
    }
    let key = *next_key;
    *next_key += 1;
    SlotOccupant {
        key,
        labels: Arc::new(labels),
    }
}

enum WindowState<G> {
    Windowless,
    Fixed(Option<Window>),
    Stamped(Option<Window>),
    Reader(G),
}

/// One group's accumulation during a build: its window source, this build's
/// values (always), and descriptors (only when `needs_schema`).
struct Group<G> {
    window: WindowState<G>,
    needs_schema: bool,
    /// Folded alongside the descriptors on a miss; see the "stored identity"
    /// note on [`GroupBuilder`]. Unused on a hit.
    walk_identity: GroupIdentityAccum,
    counter_descs: Vec<MetricDesc>,
    counter_values: Vec<Option<u64>>,
    gauge_descs: Vec<MetricDesc>,
    gauge_values: Vec<Option<i64>>,
    histogram_descs: Vec<MetricDesc>,
    histogram_values: Vec<Option<histogram::Histogram>>,
}

impl<G: ReadGuard> Group<G> {
    fn new(window: WindowState<G>, needs_schema: bool) -> Self {
        Self {
            window,
            needs_schema,
            walk_identity: GroupIdentityAccum::default(),
            counter_descs: Vec::new(),
            counter_values: Vec::new(),
            gauge_descs: Vec::new(),
            gauge_values: Vec::new(),
            histogram_descs: Vec::new(),
            histogram_values: Vec::new(),
        }
    }

    fn mark_end(&mut self) {
        if let WindowState::Reader(guard) = &mut self.window {
            guard.mark_end();
        }
    }
}

/// A group metric's `with_metadata(idx, ..)`, bound to one index.
type WithMetadata<'a> = dyn FnMut(&mut dyn FnMut(Option<&HashMap<String, String>>)) + 'a;

/// A group metric's `for_each_metadata`.
type ForEachMetadata<'a> = dyn FnMut(&mut dyn FnMut(usize, &HashMap<String, String>)) + 'a;

/// A group member's metadata: the metric's, then `id`, then the slot's own
/// (which wins on a shared key).
///
/// The base is cloned inside the callback, while the slot's metadata is in
/// hand, rather than before it. Measured against rezolus's `create_v3`, which
/// did it this way, cloning first made a tick that rebuilds a 10,000-member
/// schema 3-4.5% slower. The fallback covers a callback that is never
/// called.
fn member_metadata(
    base: &BTreeMap<String, String>,
    idx: usize,
    with_metadata: &mut WithMetadata<'_>,
) -> BTreeMap<String, String> {
    let mut built = None;
    with_metadata(&mut |m| {
        let mut metadata = base.clone();
        metadata.insert("id".to_string(), idx.to_string());
        if let Some(m) = m {
            for (k, v) in m {
                metadata.insert(k.clone(), v.clone());
            }
        }
        built = Some(metadata);
    });
    built.unwrap_or_else(|| {
        let mut metadata = base.clone();
        metadata.insert("id".to_string(), idx.to_string());
        metadata
    })
}

/// The member indices of a group metric, in ascending order.
///
/// For [`Membership::Slots`] the indices come from the metadata store, whose
/// iteration order is not stable between calls; they are collected into
/// `scratch` and sorted, so an unchanged member set gives a byte-identical
/// schema and identity from build to build. `scratch` is cleared first.
fn walk_members<'s>(
    membership: Membership<'s>,
    scratch: &'s mut Vec<usize>,
    entries: usize,
    for_each_metadata: &mut ForEachMetadata<'_>,
) -> MemberIter<'s> {
    match membership {
        Membership::Slots => {
            scratch.clear();
            for_each_metadata(&mut |idx, _| scratch.push(idx));
            scratch.sort_unstable();
            let scratch: &'s Vec<usize> = scratch;
            MemberIter::Set(scratch.iter())
        }
        Membership::Present | Membership::All => members(None, None, entries),
        Membership::Prefix(n) => members(None, Some(n), entries),
        Membership::Set(set) => members(Some(set), None, entries),
    }
}

/// Reconcile a stamped group's window across one build.
///
/// `first` is read at the group's first touch, before any of its values;
/// `latest` after all of them. A walk over the whole registry can take
/// milliseconds (rezolus measured a mean of 1.85 ms and a maximum of 5.7 ms),
/// long enough for a writer to complete a whole new acquisition in the
/// middle of it. When the two reads differ, some values just read may be
/// newer than `first` says. `first.begin_ns` is still right (nothing read is
/// older), so the result keeps it and extends the end to `latest.end_ns`,
/// bracketing every value the walk read.
///
/// `first: None, latest: Some` is a group stamped for the first time during
/// the walk: `latest` alone. `first: Some, latest: None` is not expected (a
/// window slot reads `None` only before its first stamp); `first` is kept
/// rather than dropping a real reading.
pub fn resolve_walk_window(first: Option<Window>, latest: Option<Window>) -> Option<Window> {
    match (first, latest) {
        (Some(f), Some(l)) if f == l => Some(f),
        (Some(f), Some(l)) => Some(Window::new(f.begin_ns, l.end_ns)),
        (None, Some(l)) => Some(l),
        (Some(f), None) => Some(f),
        (None, None) => None,
    }
}

/// The member indices of a registered group metric.
///
/// A prefix says "the first N", which is what a sweep over `0..n` knows. A
/// set says exactly which indices are populated, which is what a producer
/// given only part of a slot space knows, and that is rarely a prefix. The
/// difference matters most for an externally backed group (memory the
/// kernel zero-fills): it cannot hold an unwritten sentinel, so an
/// over-declared prefix publishes `0` for slots nothing measured, a wrong
/// value where the right answer is none.
enum MemberIter<'a> {
    Prefix(std::ops::Range<usize>),
    Set(std::slice::Iter<'a, usize>),
}

impl Iterator for MemberIter<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        match self {
            MemberIter::Prefix(range) => range.next(),
            MemberIter::Set(iter) => iter.next().copied(),
        }
    }
}

/// The explicit set when there is one (a caller that knows the indices knows
/// more than one that knows a count), otherwise the prefix, both clamped to
/// the backing array so a stale set or bound never walks past it.
fn members<'a>(set: Option<&'a [usize]>, bound: Option<usize>, entries: usize) -> MemberIter<'a> {
    match set {
        Some(set) => {
            let end = set.partition_point(|idx| *idx < entries);
            MemberIter::Set(set[..end].iter())
        }
        None => MemberIter::Prefix(0..bound.map_or(entries, |b| b.min(entries))),
    }
}

/// FNV-1a-128 offset basis and prime. The same algorithm as
/// [`GroupSchema::hash`] but a separate hash space: this one is a cache key
/// that is never transmitted.
const IDENTITY_FNV_OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
const IDENTITY_FNV_PRIME: u128 = 0x0000000001000000000000000000013b;

#[inline]
fn identity_fold(mut acc: u128, bytes: &[u8]) -> u128 {
    for &b in bytes {
        acc ^= b as u128;
        acc = acc.wrapping_mul(IDENTITY_FNV_PRIME);
    }
    acc
}

/// Fold a group metric's identity prefix: its registry position and the
/// version of its per-slot metadata.
///
/// The version stands in for the labels: metriken's store bumps it on every
/// mutation, so one load per metric says what byte-hashing every slot's
/// labels every build would (in rezolus that fold was 13-19% of the agent's
/// sampling CPU).
///
/// **The version is folded BEFORE the members are read**, here and in the
/// walk that builds the schema. A mutation landing between this read and a
/// member's metadata read then shows as a changed version next build and
/// costs one extra rebuild. The other order could store a schema built from
/// old metadata under a new version, which would then hit forever.
#[inline]
fn fold_group_version(acc: u128, metric_id: u64, version: u64) -> u128 {
    let h = identity_fold(acc, &metric_id.to_le_bytes());
    identity_fold(h, &version.to_le_bytes())
}

/// Per-group running identity, one accumulator per kind, so the result
/// folds them in `GroupSchema`'s (counters, gauges, histograms) order
/// whatever order the registry interleaves kinds in.
#[derive(Clone, Copy)]
struct GroupIdentityAccum {
    counters: u128,
    gauges: u128,
    histograms: u128,
}

impl Default for GroupIdentityAccum {
    fn default() -> Self {
        Self {
            counters: IDENTITY_FNV_OFFSET,
            gauges: IDENTITY_FNV_OFFSET,
            histograms: IDENTITY_FNV_OFFSET,
        }
    }
}

impl GroupIdentityAccum {
    fn finish(&self) -> (u64, u64) {
        let mut h = IDENTITY_FNV_OFFSET;
        h = identity_fold(h, &self.counters.to_le_bytes());
        h = identity_fold(h, &self.gauges.to_le_bytes());
        h = identity_fold(h, &self.histograms.to_le_bytes());
        ((h >> 64) as u64, h as u64)
    }
}

/// The first pass: fold each group's membership identity (no values, no
/// windows, no descriptor, no formatted name) and decide which groups need
/// their schema built. `true` means build; `false` means reuse the cache.
///
/// A second walk of the registry, but the registry is bounded by declared
/// metrics, not by members; what this pass avoids is allocating per member
/// for a group whose membership did not change.
///
/// A group named by `extra` is always built. The walk that follows chooses
/// members exactly as this pass does; for [`Membership::Present`] the two
/// read values separately, which [`GroupBuilder`]'s docs cover.
fn fold_group_identities<'a, R: Router>(
    cache: &FastMap<String, GroupSkeleton>,
    router: &'a R,
    metrics: &'a metriken::Metrics,
    extra: &HashSet<GroupId<'_>>,
    skip: &HashSet<GroupId<'_>>,
) -> FastMap<GroupId<'a>, Decision> {
    let mut accums: FastMap<GroupId<'a>, GroupIdentityAccum> = FastMap::default();
    let mut idx_scratch: Vec<usize> = Vec::new();

    for (metric_id, metric) in metrics.iter().enumerate() {
        let Some(value) = metric.value() else {
            continue;
        };
        let Some(route) = router.route(metric) else {
            continue;
        };
        if skip.contains(&route.group) {
            continue;
        }
        let registered = route.membership.registered();

        let accum = accums.entry(route.group).or_default();
        let metric_id = metric_id as u64;

        match value {
            Value::Counter(_) => {
                accum.counters = identity_fold(accum.counters, &metric_id.to_le_bytes());
            }
            Value::Gauge(_) => {
                accum.gauges = identity_fold(accum.gauges, &metric_id.to_le_bytes());
            }
            Value::CounterGroup(g) => {
                accum.counters =
                    fold_group_version(accum.counters, metric_id, g.metadata_version());
                let members =
                    walk_members(route.membership, &mut idx_scratch, g.entries(), &mut |f| {
                        g.for_each_metadata(f)
                    });
                for idx in members {
                    if !registered && !matches!(g.counter_value(idx), Some(v) if v != 0) {
                        continue;
                    }
                    accum.counters = identity_fold(accum.counters, &(idx as u64).to_le_bytes());
                }
            }
            Value::GaugeGroup(g) => {
                accum.gauges = fold_group_version(accum.gauges, metric_id, g.metadata_version());
                let members =
                    walk_members(route.membership, &mut idx_scratch, g.entries(), &mut |f| {
                        g.for_each_metadata(f)
                    });
                for idx in members {
                    if !registered && g.gauge_value(idx).is_none() {
                        continue;
                    }
                    accum.gauges = identity_fold(accum.gauges, &(idx as u64).to_le_bytes());
                }
            }
            Value::Histogram(h) if registered || h.load().is_some() => {
                accum.histograms = identity_fold(accum.histograms, &metric_id.to_le_bytes());
            }
            _ => {}
        }
    }

    accums
        .into_iter()
        .map(|(key, accum)| {
            let hit = if extra.contains(&key) {
                None
            } else {
                cache
                    .get(&key.wire_name())
                    .filter(|cached| cached.identity == accum.finish())
            };
            let decision = match hit {
                Some(cached) => Decision {
                    needs_schema: false,
                    sizes: Some((
                        cached.schema.counters.len(),
                        cached.schema.gauges.len(),
                        cached.schema.histograms.len(),
                    )),
                },
                None => Decision::default(),
            };
            (key, decision)
        })
        .collect()
}

/// What the first pass decided for one group.
#[derive(Clone, Copy, Debug)]
struct Decision {
    /// The group's membership changed (or it has no cached schema, or it is
    /// an extra group): the walk builds its schema.
    needs_schema: bool,
    /// On a hit, the cached schema's counter, gauge and histogram counts, to
    /// size the walk's value vectors.
    sizes: Option<(usize, usize, usize)>,
}

impl Default for Decision {
    fn default() -> Self {
        Self {
            needs_schema: true,
            sizes: None,
        }
    }
}

/// Whether `metric` is a [`metriken::CounterFamily`] or
/// [`metriken::GaugeFamily`]. A family's members are its live slots, so a
/// router gives it [`Membership::Slots`].
pub fn is_family(metric: &MetricEntry) -> bool {
    metric
        .as_any()
        .is_some_and(|any| any.is::<metriken::CounterFamily>() || any.is::<metriken::GaugeFamily>())
}

/// A router for a producer with no group registry of its own.
///
/// Every exposed metric goes to `{namespace}/{acq_group}` when its static
/// metadata names a group ([`GROUP_METADATA_KEY`]) and to `{namespace}/main`
/// otherwise. A family's members are its slots; a metric in a named group
/// has registered membership over the whole backing array; any other metric
/// has value-derived membership. No group has a window.
#[derive(Clone, Debug)]
pub struct DefaultRouter {
    namespace: String,
}

impl DefaultRouter {
    pub fn new(namespace: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
        }
    }
}

impl Router for DefaultRouter {
    type Guard = NoGuard;

    fn route<'a>(&'a self, metric: &'a MetricEntry) -> Option<Route<'a>> {
        let named = metric.metadata().get(GROUP_METADATA_KEY);
        let membership = if is_family(metric) {
            Membership::Slots
        } else if named.is_some() {
            Membership::All
        } else {
            Membership::Present
        };
        Some(Route {
            group: GroupId::new(&self.namespace, named.unwrap_or("main")),
            membership,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_walk_window_unchanged_returns_the_same_window() {
        let w = Window::new(1_000, 2_000);
        assert_eq!(resolve_walk_window(Some(w), Some(w)), Some(w));
    }

    #[test]
    fn resolve_walk_window_changed_returns_the_union() {
        let first = Window::new(1_000, 2_000);
        let latest = Window::new(3_000, 4_000);
        assert_eq!(
            resolve_walk_window(Some(first), Some(latest)),
            Some(Window::new(1_000, 4_000)),
            "keeps first's begin, extends to latest's end"
        );
    }

    #[test]
    fn resolve_walk_window_both_none_is_none() {
        assert_eq!(resolve_walk_window(None, None), None);
    }

    #[test]
    fn resolve_walk_window_stamped_for_the_first_time_mid_walk_uses_latest() {
        let latest = Window::new(5_000, 6_000);
        assert_eq!(
            resolve_walk_window(None, Some(latest)),
            Some(latest),
            "nothing to union with a never-yet-stamped first read"
        );
    }

    #[test]
    fn resolve_walk_window_transient_unstamped_latest_keeps_first() {
        let first = Window::new(1_000, 2_000);
        assert_eq!(resolve_walk_window(Some(first), None), Some(first));
    }

    /// An explicit member set walks exactly those indices, and a bound walks
    /// the prefix. A group whose members are slots 16-31 cannot say so with a
    /// bound; for an externally backed group an over-declared prefix
    /// publishes zeros nothing measured.
    #[test]
    fn an_explicit_member_set_walks_only_its_own_indices() {
        let prefix: Vec<usize> = members(None, Some(4), 32).collect();
        assert_eq!(prefix, vec![0, 1, 2, 3]);

        let all: Vec<usize> = members(None, None, 3).collect();
        assert_eq!(all, vec![0, 1, 2]);

        let set = [16usize, 17, 18, 31];
        let sparse: Vec<usize> = members(Some(&set), None, 32).collect();
        assert_eq!(sparse, vec![16, 17, 18, 31]);

        // The set wins over a bound.
        let both: Vec<usize> = members(Some(&set), Some(2), 32).collect();
        assert_eq!(both, vec![16, 17, 18, 31]);
    }

    /// A member set is clamped to the backing array, like a bound.
    #[test]
    fn a_member_set_never_walks_past_the_backing_array() {
        let set = [0usize, 1, 2, 99];
        let walked: Vec<usize> = members(Some(&set), None, 3).collect();
        assert_eq!(walked, vec![0, 1, 2], "index 99 is not in a 3-entry array");

        let empty: Vec<usize> = members(Some(&set), None, 0).collect();
        assert!(empty.is_empty());
    }

    #[test]
    fn slots_are_walked_in_ascending_order() {
        let mut scratch = Vec::new();
        let walked: Vec<usize> = walk_members(Membership::Slots, &mut scratch, 64, &mut |f| {
            let empty = HashMap::new();
            for idx in [40, 3, 17] {
                f(idx, &empty);
            }
        })
        .collect();
        assert_eq!(walked, vec![3, 17, 40]);
    }
}
