//! A producer's groups as dendro replication frames: the transport-free half
//! of a live metrics stream.
//!
//! A producer is one dendro *source*: one clock domain, one identity, one
//! sequence of observations. A subscription is dendro's preamble, a
//! [`Frame::Handshake`] once, then one [`Frame::Rows`] per interval whose
//! rows are encoded [`WalGroupRow`]s, [`WalLongRow`]s, and [`Occupant`] rows
//! on `<group>/occupants`. Moved from rezolus's agent
//! (`frames.rs`); the transport (an HTTP route, its timer, which groups a
//! subscriber asked for) stays with the caller.
//!
//! # No identity index
//!
//! No [`Frame::Index`] is sent. For a group sent wide, what a slot means
//! travels in the group's schema (each member's labels, `__uid__` included),
//! which the payload carries whenever it changes. For a group sent long, it
//! travels on `<group>/occupants` (see below). Every rows frame names dendro's
//! [`NO_INDEX_STATE`], which a subscriber always resolves.
//!
//! # Time is the producer's
//!
//! Rows are stamped by the producer, because only the producer knows when
//! the values were read: a pass may be cached and served later, and on a
//! stream there is no consumer request at all, only the send. dendro's
//! FORMAT.md §5 makes one source one clock domain, and the anchor that pins
//! the timeline (`clock_anchor_wall_ns`) is a handshake field. If a
//! subscriber stamped, two subscribers to one producer would build two
//! timelines for one source, which could not be merged.
//!
//! So `ts` is anchored (`anchor + monotonic elapsed`, [`metriken::epoch`]):
//! wall-clock magnitude, strictly increasing through a clock step.
//! `wall_offset` is wall minus `ts` at the read, so `ts + wall_offset` is the
//! wall clock and a step is visible rather than absorbed. The anchor is one
//! per process, so every subscription is told the same timeline.
//!
//! # The schema travels inside the payload
//!
//! A [`WalRow`] has no envelope: `stream`, `ts`, `wall_offset` and an opaque
//! payload. A group's schema therefore goes inside the payload, in
//! [`WalGroupRow::schema`], on the first row of a stream and whenever the
//! schema's hash changes, and is left out otherwise.
//!
//! # Groups of counter groups and gauge groups are sent long
//!
//! A [`LongGroupSnapshot`] is sent as a [`WalLongRow`] on the group's stream,
//! keyed by the producer's occupant keys, whose schema is the group's metric
//! columns and changes only when its metrics do. Before it, on
//! `<group>/occupants`, the subscription is sent an [`Occupant`] (key and
//! labels) for each occupant that was not in the previous row of this group
//! sent to this subscription: new
//! occupants, and every occupant on the subscription's first row. A
//! departure is not sent; the occupant is absent from later rows. A group
//! can change form between rows; the subscriber reads the form from each
//! row, and a change of form sends the row's schema and, for a long row,
//! every occupant again.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use dendro::archive::WalRow;
use dendro::replicate::{Frame, NO_INDEX_STATE};
use metriken_exposition::group_builder::{LongGroupSnapshot, StreamGroup};
use metriken_exposition::GroupSnapshot;
use metriken_segment::occupants::{self, Occupant};
use metriken_segment::schema::GroupSchema;
#[cfg(doc)]
use metriken_segment::wal::WalGroupRow;
use metriken_segment::wal::{
    decode_wal_long_row, encode_wal_group_row, encode_wal_group_row_with_schema,
    encode_wal_long_row, LongOccupant, WalLongRow,
};

/// The ordinal a producer's own source takes. A producer is one source, so
/// it is always this; the field exists because a connection may carry
/// several.
pub const SOURCE: u32 = 0;

/// An occupant's key and its labels, as a long row lists them.
pub type KeyedLabels = (u64, Arc<BTreeMap<String, String>>);

/// One group's row for one pass, as a [`FrameProducer`] reads it.
///
/// Implemented by [`EncodedGroup`]. A producer that already keeps its rows in
/// another type (with fields of its own) implements it for that type.
pub trait StreamRow {
    /// The dendro stream: the group's name.
    fn stream(&self) -> &str;
    /// [`GroupSchema::hash`] of the schema the values align with.
    fn schema_hash(&self) -> (u64, u64);
    /// The schema, when the row has it to hand. A row without one is sent
    /// without one, and a subscriber that does not already hold the schema
    /// skips it.
    fn schema(&self) -> Option<&GroupSchema>;
    /// The encoded [`WalGroupRow`] without its schema, or for a long row
    /// the encoded [`WalLongRow`] without its schema.
    fn payload(&self) -> &[u8];
    /// For a long row, each occupant present: its key and labels, in the
    /// row's order. `None` for a [`WalGroupRow`].
    fn occupants(&self) -> Option<&[KeyedLabels]> {
        None
    }
}

/// Each group's schema in the segment format, converted once per schema
/// hash and shared between passes.
///
/// A snapshot carries every group's schema on every pass; this converts one
/// only when its hash changes. Keep one per producer, across passes.
///
/// One entry per group name, holding the latest hash. An entry stays until
/// [`retain`](Self::retain) drops it; a producer whose group names are
/// unbounded calls `retain` with the names it still has.
#[derive(Debug, Default)]
pub struct SchemaCache {
    by_stream: HashMap<String, ((u64, u64), Arc<GroupSchema>)>,
}

impl SchemaCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// `g`'s schema, converted if its hash is not the one cached for its
    /// name. `None` when the snapshot carries no schema.
    pub fn schema(&mut self, g: &GroupSnapshot) -> Option<Arc<GroupSchema>> {
        let source = g.schema.as_ref()?;
        if let Some((hash, schema)) = self.by_stream.get(&g.name) {
            if *hash == g.schema_hash {
                return Some(Arc::clone(schema));
            }
        }
        let schema = Arc::new(GroupSchema::from(source.as_ref()));
        self.by_stream
            .insert(g.name.clone(), (g.schema_hash, Arc::clone(&schema)));
        Some(schema)
    }

    /// A long group's columns in the segment format, converted if its hash
    /// is not the one cached for its name.
    pub fn columns(&mut self, g: &LongGroupSnapshot) -> Arc<GroupSchema> {
        if let Some((hash, schema)) = self.by_stream.get(&g.name) {
            if *hash == g.columns_hash {
                return Arc::clone(schema);
            }
        }
        let schema = Arc::new(GroupSchema::from(g.columns.as_ref()));
        self.by_stream
            .insert(g.name.clone(), (g.columns_hash, Arc::clone(&schema)));
        schema
    }

    /// Keep only the groups whose names `keep` accepts. A group dropped here
    /// is converted again if it reappears.
    pub fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.by_stream.retain(|name, _| keep(name));
    }

    /// How many groups the cache holds.
    pub fn len(&self) -> usize {
        self.by_stream.len()
    }

    /// Whether the cache holds no groups.
    pub fn is_empty(&self) -> bool {
        self.by_stream.is_empty()
    }
}

/// A [`GroupSnapshot`] encoded once per pass, shared by every subscriber.
///
/// Encoding is per pass rather than per subscriber: what differs between
/// subscribers is only whether each row carries its schema.
#[derive(Clone, Debug, PartialEq)]
pub struct EncodedGroup {
    pub stream: String,
    /// The group's acquisition window, `(begin_ns, end_ns)`.
    pub window: Option<(u64, u64)>,
    pub schema_hash: (u64, u64),
    pub schema: Option<Arc<GroupSchema>>,
    /// The encoded [`WalGroupRow`] with `schema: None`.
    pub row: Vec<u8>,
}

impl EncodedGroup {
    pub fn encode(g: &GroupSnapshot, schemas: &mut SchemaCache) -> Result<Self, String> {
        Ok(Self {
            stream: g.name.clone(),
            window: g.window.map(|w| (w.begin_ns, w.end_ns)),
            schema_hash: g.schema_hash,
            schema: schemas.schema(g),
            row: encode_wal_group_row(&metriken_exposition::wal_group_row(g, None))?,
        })
    }
}

/// Every group of a pass, encoded.
pub fn encode_groups<'a>(
    groups: impl IntoIterator<Item = &'a GroupSnapshot>,
    schemas: &mut SchemaCache,
) -> Result<Vec<EncodedGroup>, String> {
    groups
        .into_iter()
        .map(|g| EncodedGroup::encode(g, schemas))
        .collect()
}

/// A [`LongGroupSnapshot`] encoded once per pass, shared by every
/// subscriber.
#[derive(Clone, Debug, PartialEq)]
pub struct EncodedLongGroup {
    pub stream: String,
    pub window: Option<(u64, u64)>,
    /// [`GroupSchema::hash`] of `columns`.
    pub columns_hash: (u64, u64),
    pub columns: Arc<GroupSchema>,
    /// Each occupant present: its key and labels, in `row`'s order.
    pub occupants: Vec<KeyedLabels>,
    /// The encoded [`WalLongRow`] with `schema: None`, keyed by occupant key.
    pub row: Vec<u8>,
}

impl EncodedLongGroup {
    pub fn encode(g: &LongGroupSnapshot, schemas: &mut SchemaCache) -> Result<Self, String> {
        let row = WalLongRow {
            schema_hash: g.columns_hash,
            schema: None,
            window: g.window.map(|w| (w.begin_ns, w.end_ns)),
            occupants: g
                .occupants
                .iter()
                .map(|o| LongOccupant {
                    occupant: o.key,
                    counters: o.counters.clone(),
                    gauges: o.gauges.clone(),
                    histograms: Vec::new(),
                })
                .collect(),
        };
        Ok(Self {
            stream: g.name.clone(),
            window: row.window,
            columns_hash: g.columns_hash,
            columns: schemas.columns(g),
            occupants: g
                .occupants
                .iter()
                .map(|o| (o.key, Arc::clone(&o.labels)))
                .collect(),
            row: encode_wal_long_row(&row)?,
        })
    }
}

/// One group of a pass from [`GroupBuilder::build_stream`], encoded.
///
/// [`GroupBuilder::build_stream`]: metriken_exposition::group_builder::GroupBuilder::build_stream
#[derive(Clone, Debug, PartialEq)]
pub enum EncodedStreamGroup {
    Wide(EncodedGroup),
    Long(EncodedLongGroup),
}

impl EncodedStreamGroup {
    pub fn encode(g: &StreamGroup, schemas: &mut SchemaCache) -> Result<Self, String> {
        Ok(match g {
            StreamGroup::Wide(g) => Self::Wide(EncodedGroup::encode(g, schemas)?),
            StreamGroup::Long(g) => Self::Long(EncodedLongGroup::encode(g, schemas)?),
        })
    }
}

impl StreamRow for EncodedLongGroup {
    fn stream(&self) -> &str {
        &self.stream
    }

    fn schema_hash(&self) -> (u64, u64) {
        self.columns_hash
    }

    fn schema(&self) -> Option<&GroupSchema> {
        Some(&self.columns)
    }

    fn payload(&self) -> &[u8] {
        &self.row
    }

    fn occupants(&self) -> Option<&[KeyedLabels]> {
        Some(&self.occupants)
    }
}

impl StreamRow for EncodedStreamGroup {
    fn stream(&self) -> &str {
        match self {
            Self::Wide(g) => g.stream(),
            Self::Long(g) => g.stream(),
        }
    }

    fn schema_hash(&self) -> (u64, u64) {
        match self {
            Self::Wide(g) => g.schema_hash(),
            Self::Long(g) => g.schema_hash(),
        }
    }

    fn schema(&self) -> Option<&GroupSchema> {
        match self {
            Self::Wide(g) => g.schema(),
            Self::Long(g) => g.schema(),
        }
    }

    fn payload(&self) -> &[u8] {
        match self {
            Self::Wide(g) => g.payload(),
            Self::Long(g) => g.payload(),
        }
    }

    fn occupants(&self) -> Option<&[KeyedLabels]> {
        match self {
            Self::Wide(g) => g.occupants(),
            Self::Long(g) => g.occupants(),
        }
    }
}

impl StreamRow for EncodedGroup {
    fn stream(&self) -> &str {
        &self.stream
    }

    fn schema_hash(&self) -> (u64, u64) {
        self.schema_hash
    }

    fn schema(&self) -> Option<&GroupSchema> {
        self.schema.as_deref()
    }

    fn payload(&self) -> &[u8] {
        &self.row
    }
}

/// Builds one subscription's frames.
///
/// One per subscription, because the state it keeps is what THIS subscriber
/// has been sent. Two subscribers that connected at different times hold
/// different schemas, and a producer shared between them would tell the
/// newer one a schema it never saw was already known.
pub struct FrameProducer {
    uuid: String,
    clock_anchor_wall_ns: i64,
    labels: BTreeMap<String, String>,
    metadata: BTreeMap<String, String>,
    /// The schema hash this subscriber was last sent, by stream.
    sent_schemas: BTreeMap<String, (u64, u64)>,
    /// Per long stream, the occupant keys in the last row this subscriber
    /// was sent.
    sent_occupants: HashMap<String, HashSet<u64>>,
    handshake_sent: bool,
}

impl FrameProducer {
    /// This process as a source: the uuid is [`metriken::epoch::producer_epoch`],
    /// the anchor is [`metriken::epoch::clock_anchor_wall_ns`], and
    /// `metadata` gains dendro's `keys::PRODUCER_EPOCH` with the same epoch.
    ///
    /// The epoch is the source uuid because the two mean the same thing, this
    /// producer until its counters restart, and one value for both means a
    /// subscriber cannot see them disagree.
    pub fn new(labels: BTreeMap<String, String>, mut metadata: BTreeMap<String, String>) -> Self {
        let epoch = metriken::epoch::producer_epoch().to_string();
        metadata.insert(dendro::keys::PRODUCER_EPOCH.to_string(), epoch.clone());
        Self::for_source(
            epoch,
            metriken::epoch::clock_anchor_wall_ns(),
            labels,
            metadata,
        )
    }

    /// A source named explicitly, with its handshake fields as given.
    pub fn for_source(
        uuid: String,
        clock_anchor_wall_ns: i64,
        labels: BTreeMap<String, String>,
        metadata: BTreeMap<String, String>,
    ) -> Self {
        Self {
            uuid,
            clock_anchor_wall_ns,
            labels,
            metadata,
            sent_schemas: BTreeMap::new(),
            sent_occupants: HashMap::new(),
            handshake_sent: false,
        }
    }

    /// The opening frame. Sent once per connection, before anything else.
    pub fn handshake(&mut self) -> Frame {
        self.handshake_sent = true;
        Frame::Handshake {
            source: SOURCE,
            uuid: Some(self.uuid.clone()),
            labels: self.labels.clone(),
            metadata: self.metadata.clone(),
            clock_anchor_wall_ns: self.clock_anchor_wall_ns,
            // A live producer's source is open for as long as it runs; the
            // subscriber's `finish()` closes its copy.
            complete: false,
        }
    }

    /// The bytes that open a subscription: dendro's preamble, so a consumer
    /// can reject a stream it cannot read before decoding a frame, then the
    /// handshake.
    pub fn opening(&mut self) -> dendro::Result<Vec<u8>> {
        let mut out = Vec::new();
        dendro::replicate::wire::write_preamble(&mut out)?;
        dendro::replicate::wire::encode_frame(&self.handshake(), &mut out)?;
        Ok(out)
    }

    /// Whether [`handshake`](Self::handshake) has been called.
    pub fn has_sent_handshake(&self) -> bool {
        self.handshake_sent
    }

    /// One interval's rows frame.
    ///
    /// `ts` and `wall_offset` are the pass's own stamp, carried through
    /// rather than read here: a frame sent now can describe a pass read up to
    /// a cache lifetime ago, and stamping at the send would date every
    /// reading to when it was sent.
    ///
    /// `seq` is the subscription's interval index, not a frame count. dendro
    /// uses it only to notice `seq > last + 1`, which an interval index
    /// satisfies, and a gap then says an interval produced no frame.
    ///
    /// Filter `rows` to what this subscriber should receive before passing
    /// them: a schema is recorded as sent when a row carrying it is built,
    /// so a row filtered out afterwards would leave its group referencing a
    /// schema this subscriber never received. Likewise an occupant is
    /// recorded as sent when its occupants row is built, so a long row
    /// filtered out afterwards leaves its new occupants without labels for
    /// as long as they stay present.
    pub fn interval<'r, R: StreamRow + 'r>(
        &mut self,
        rows: impl IntoIterator<Item = &'r R>,
        ts: i64,
        wall_offset: i64,
        seq: u64,
    ) -> Frame {
        let mut out = Vec::new();
        for row in rows {
            let long = row.occupants();
            // A change of form anchors the row's schema again, since a hash
            // sent for one form says nothing to a reader of the other, and a
            // group that turns long again describes its occupants afresh.
            let was_long = self.sent_occupants.contains_key(row.stream());
            if was_long != long.is_some() {
                self.sent_schemas.remove(row.stream());
            }
            if long.is_none() {
                self.sent_occupants.remove(row.stream());
            }
            if let Some(present) = long {
                // The occupants not yet sent to this subscriber go first, so
                // their labels are known when the row is read.
                let sent = self
                    .sent_occupants
                    .entry(row.stream().to_string())
                    .or_default();
                let new: Vec<Occupant> = present
                    .iter()
                    .filter(|(key, _)| !sent.contains(key))
                    .map(|(key, labels)| Occupant {
                        occupant: *key,
                        labels: labels.as_ref().clone(),
                    })
                    .collect();
                sent.clear();
                sent.extend(present.iter().map(|(key, _)| *key));
                if !new.is_empty() {
                    out.push(WalRow {
                        stream: occupants::stream_of(row.stream()),
                        ts,
                        wall_offset,
                        row: occupants::encode_wal_row(&new),
                    });
                }
            }
            let payload = match self.sent_schemas.get(row.stream()) {
                Some(hash) if *hash == row.schema_hash() => row.payload().to_vec(),
                _ => {
                    self.sent_schemas
                        .insert(row.stream().to_string(), row.schema_hash());
                    match (row.schema(), long.is_some()) {
                        (Some(schema), false) => with_schema(row.payload(), schema),
                        (Some(schema), true) => with_long_schema(row.payload(), schema),
                        // Nothing to anchor with: sent as is, and a
                        // subscriber without the schema skips it, as it
                        // does any row whose hash it cannot resolve.
                        (None, _) => row.payload().to_vec(),
                    }
                }
            };
            out.push(WalRow {
                stream: row.stream().to_string(),
                ts,
                wall_offset,
                row: payload,
            });
        }
        let rows = out;

        Frame::Rows {
            source: SOURCE,
            seq,
            index_state: NO_INDEX_STATE,
            rows,
        }
    }

    /// An interval that produced no new reading: a rows frame with no rows,
    /// which says the interval elapsed with nothing new and keeps the
    /// connection alive.
    pub fn empty_interval(&self, seq: u64) -> Frame {
        Frame::Rows {
            source: SOURCE,
            seq,
            index_state: NO_INDEX_STATE,
            rows: Vec::new(),
        }
    }
}

/// Put `schema` into an encoded [`WalGroupRow`], leaving everything else as
/// it is.
///
/// Spliced into the encoded row rather than built again from the snapshot:
/// the values are already encoded, and deriving them twice could produce
/// different values. A payload that does not decode is returned unchanged; the
/// subscriber skips a row it cannot read, and losing one row is better than
/// ending the subscription.
fn with_schema(payload: &[u8], schema: &GroupSchema) -> Vec<u8> {
    encode_wal_group_row_with_schema(payload, schema).unwrap_or_else(|_| payload.to_vec())
}

/// [`with_schema`] for an encoded [`WalLongRow`]. Decoded and encoded again:
/// a long row's schema changes only when its group's metrics do.
fn with_long_schema(payload: &[u8], schema: &GroupSchema) -> Vec<u8> {
    decode_wal_long_row(payload)
        .and_then(|mut row| {
            row.schema = Some(schema.clone());
            encode_wal_long_row(&row)
        })
        .unwrap_or_else(|_| payload.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use metriken_segment::schema::MetricDesc;
    use metriken_segment::wal::{decode_wal_group_row, WalGroupRow};

    const STREAM: &str = "cpu_usage/cpu_usage_task";
    const UUID: &str = "11111111-2222-4333-8444-555555555555";

    fn schema(n: usize) -> GroupSchema {
        GroupSchema {
            counters: (0..n)
                .map(|i| MetricDesc {
                    name: format!("0x{i}"),
                    metadata: [("metric".to_string(), "cpu_usage_user".to_string())].into(),
                })
                .collect(),
            gauges: Vec::new(),
            histograms: Vec::new(),
        }
    }

    fn row(n: usize) -> EncodedGroup {
        EncodedGroup {
            stream: STREAM.to_string(),
            window: Some((1_000, 2_000)),
            schema_hash: schema(n).hash(),
            schema: Some(Arc::new(schema(n))),
            row: encode_wal_group_row(&WalGroupRow {
                schema_hash: schema(n).hash(),
                schema: None,
                window: Some((1_000, 2_000)),
                counters: (0..n).map(|i| Some(i as u64)).collect(),
                gauges: Vec::new(),
                histograms: Vec::new(),
            })
            .unwrap(),
        }
    }

    fn producer() -> FrameProducer {
        FrameProducer::for_source(
            UUID.to_string(),
            1_700_000_000_000_000_000,
            [("source".to_string(), "test".to_string())].into(),
            BTreeMap::new(),
        )
    }

    fn rows_of(frame: &Frame) -> &[WalRow] {
        let Frame::Rows { rows, .. } = frame else {
            panic!("a rows frame")
        };
        rows
    }

    fn group(name: &str, n: usize) -> GroupSnapshot {
        let desc = |i: usize| metriken_exposition::MetricDesc {
            name: format!("{i}"),
            metadata: [("metric".to_string(), "cpu_usage_user".to_string())].into(),
        };
        let schema = metriken_exposition::GroupSchema {
            counters: (0..n).map(desc).collect(),
            gauges: Vec::new(),
            histograms: Vec::new(),
        };
        GroupSnapshot {
            name: name.to_string(),
            schema_hash: schema.hash(),
            schema: Some(Arc::new(schema)),
            window: None,
            counters: vec![Some(1); n],
            gauges: Vec::new(),
            histograms: Vec::new(),
        }
    }

    /// A pass whose group kept its hash reuses the converted schema; a new
    /// hash converts again, and other groups keep theirs.
    #[test]
    fn a_schema_is_converted_once_per_hash() {
        let mut cache = SchemaCache::new();
        let first = cache.schema(&group(STREAM, 3)).unwrap();
        let other = cache
            .schema(&group("cpu_usage/cpu_usage_cgroup", 2))
            .unwrap();
        let again = cache.schema(&group(STREAM, 3)).unwrap();
        assert!(Arc::ptr_eq(&first, &again), "same hash, same conversion");
        assert_eq!(
            *first,
            GroupSchema::from(group(STREAM, 3).schema.unwrap().as_ref())
        );

        let changed = cache.schema(&group(STREAM, 4)).unwrap();
        assert!(!Arc::ptr_eq(&first, &changed));
        assert_eq!(changed.counters.len(), 4);
        let other_again = cache
            .schema(&group("cpu_usage/cpu_usage_cgroup", 2))
            .unwrap();
        assert!(Arc::ptr_eq(&other, &other_again));

        let mut bare = group(STREAM, 4);
        bare.schema = None;
        assert!(cache.schema(&bare).is_none());
    }

    /// `retain` drops the groups it rejects and keeps the rest; a dropped
    /// group is converted again when it reappears.
    #[test]
    fn retain_drops_the_groups_it_rejects() {
        let mut cache = SchemaCache::new();
        let kept = cache.schema(&group(STREAM, 3)).unwrap();
        let gone = cache.schema(&group("cgroup/one", 2)).unwrap();
        assert_eq!(cache.len(), 2);

        cache.retain(|name| name == STREAM);
        assert_eq!(cache.len(), 1);
        assert!(Arc::ptr_eq(
            &kept,
            &cache.schema(&group(STREAM, 3)).unwrap()
        ));
        let again = cache.schema(&group("cgroup/one", 2)).unwrap();
        assert!(!Arc::ptr_eq(&gone, &again), "converted again");
        assert_eq!(*gone, *again);
    }

    /// The first row of a stream carries its schema inside the payload, or
    /// the subscriber has nothing to resolve `schema_hash` against.
    #[test]
    fn the_first_row_of_a_stream_carries_its_schema_inside_the_payload() {
        let mut p = producer();
        let frame = p.interval([&row(3)], 2_000, 0, 7);
        let decoded = decode_wal_group_row(&rows_of(&frame)[0].row).unwrap();
        assert_eq!(decoded.schema.as_ref().map(|s| s.counters.len()), Some(3));
        assert_eq!(decoded.schema_hash, schema(3).hash());
        assert_eq!(decoded.counters, vec![Some(0), Some(1), Some(2)]);
    }

    fn long(keys: &[u64]) -> EncodedLongGroup {
        let columns = schema(1);
        EncodedLongGroup {
            stream: STREAM.to_string(),
            window: None,
            columns_hash: columns.hash(),
            columns: Arc::new(columns.clone()),
            occupants: keys
                .iter()
                .map(|k| {
                    (
                        *k,
                        Arc::new(BTreeMap::from([("id".to_string(), k.to_string())])),
                    )
                })
                .collect(),
            row: encode_wal_long_row(&WalLongRow {
                schema_hash: columns.hash(),
                schema: None,
                window: None,
                occupants: keys
                    .iter()
                    .map(|k| LongOccupant {
                        occupant: *k,
                        counters: vec![Some(*k)],
                        gauges: Vec::new(),
                        histograms: Vec::new(),
                    })
                    .collect(),
            })
            .unwrap(),
        }
    }

    /// A long row is preceded by the occupants this subscriber has not been
    /// told about: all of them first, then only those that were not in the
    /// previous row. The columns are anchored once.
    #[test]
    fn a_long_row_follows_the_occupants_it_introduces() {
        let mut p = producer();
        let described = |frame: &Frame| -> (Vec<u64>, bool) {
            let rows = rows_of(frame);
            let last = rows.last().unwrap();
            assert_eq!(last.stream, STREAM);
            let anchored = decode_wal_long_row(&last.row).unwrap().schema.is_some();
            let keys = match rows.len() {
                1 => Vec::new(),
                2 => {
                    assert_eq!(rows[0].stream, occupants::stream_of(STREAM));
                    occupants::decode_wal_row(&rows[0].row)
                        .unwrap()
                        .into_iter()
                        .map(|o| o.occupant)
                        .collect()
                }
                n => panic!("{n} rows"),
            };
            (keys, anchored)
        };
        assert_eq!(
            described(&p.interval([&long(&[1, 2])], 1, 0, 1)),
            (vec![1, 2], true)
        );
        assert_eq!(
            described(&p.interval([&long(&[1, 2])], 2, 0, 2)),
            (vec![], false)
        );
        assert_eq!(
            described(&p.interval([&long(&[2, 3])], 3, 0, 3)),
            (vec![3], false)
        );
        // A key that left and came back is described again.
        assert_eq!(
            described(&p.interval([&long(&[1, 3])], 4, 0, 4)),
            (vec![1], false)
        );
        // A row sent wide in between makes the next long row describe
        // everything again.
        p.interval([&row(1)], 5, 0, 5);
        assert_eq!(
            described(&p.interval([&long(&[1, 3])], 6, 0, 6)),
            (vec![1, 3], true)
        );
        // Another subscription is told everything.
        let mut q = producer();
        assert_eq!(
            described(&q.interval([&long(&[1, 3])], 4, 0, 1)),
            (vec![1, 3], true)
        );
    }

    #[test]
    fn a_schema_already_sent_is_not_sent_again() {
        let mut p = producer();
        p.interval([&row(3)], 2_000, 0, 7);
        let frame = p.interval([&row(3)], 3_000, 0, 8);
        let decoded = decode_wal_group_row(&rows_of(&frame)[0].row).unwrap();
        assert!(decoded.schema.is_none(), "the subscriber already has it");
        assert_eq!(decoded.schema_hash, schema(3).hash());
    }

    /// Tracked per stream by hash: a group whose membership moved sends its
    /// schema again.
    #[test]
    fn a_changed_schema_is_sent_again() {
        let mut p = producer();
        p.interval([&row(3)], 2_000, 0, 7);
        let frame = p.interval([&row(4)], 3_000, 0, 8);
        let decoded = decode_wal_group_row(&rows_of(&frame)[0].row).unwrap();
        assert_eq!(decoded.schema.map(|s| s.counters.len()), Some(4));
    }

    /// Two subscriptions keep separate schema state: the second one is sent
    /// the schema although the first already has it.
    #[test]
    fn each_subscription_is_sent_the_schema_once() {
        let mut first = producer();
        first.interval([&row(3)], 2_000, 0, 7);
        let mut second = producer();
        let frame = second.interval([&row(3)], 3_000, 0, 8);
        let decoded = decode_wal_group_row(&rows_of(&frame)[0].row).unwrap();
        assert!(decoded.schema.is_some());
    }

    /// A frame carries the stamp of the pass it describes, not of the send;
    /// `ts + wall_offset` is the wall clock at the read.
    #[test]
    fn a_frame_carries_the_stamp_of_the_pass_not_of_the_send() {
        let mut p = producer();
        let pass_ts = 1_700_000_000_000_000_000i64;
        let pass_offset = 5_000_000_000i64;
        let frame = p.interval([&row(3)], pass_ts, pass_offset, 7);
        let row = &rows_of(&frame)[0];
        assert_eq!(row.ts, pass_ts);
        assert_eq!(row.ts + row.wall_offset, pass_ts + pass_offset);
        assert_ne!(
            row.ts,
            metriken::epoch::anchored_ts(std::time::Instant::now())
        );
    }

    #[test]
    fn a_rows_frame_names_no_index_state() {
        let mut p = producer();
        let Frame::Rows { index_state, .. } = p.interval([&row(3)], 2_000, 0, 7) else {
            panic!("a rows frame")
        };
        assert_eq!(index_state, NO_INDEX_STATE);
        let Frame::Rows { index_state, .. } = p.empty_interval(8) else {
            panic!("a rows frame")
        };
        assert_eq!(index_state, NO_INDEX_STATE);
    }

    #[test]
    fn an_empty_interval_is_a_rows_frame_with_no_rows() {
        let Frame::Rows { seq, rows, .. } = producer().empty_interval(99) else {
            panic!("a rows frame")
        };
        assert_eq!(seq, 99, "the interval index, so a gap is still visible");
        assert!(rows.is_empty());
    }

    /// Two subscriptions to one process are told the same timeline and the
    /// same source: the process's anchor and epoch.
    #[test]
    fn every_subscription_is_told_the_same_source_timeline() {
        let handshake = || {
            let Frame::Handshake {
                clock_anchor_wall_ns,
                uuid,
                metadata,
                complete,
                ..
            } = FrameProducer::new(BTreeMap::new(), BTreeMap::new()).handshake()
            else {
                panic!("a handshake")
            };
            (clock_anchor_wall_ns, uuid, metadata, complete)
        };
        let (a, b) = (handshake(), handshake());
        assert_eq!(a, b, "two subscriptions, one source, one timeline");
        assert_eq!(a.0, metriken::epoch::clock_anchor_wall_ns());
        assert_eq!(a.1.as_deref(), Some(metriken::epoch::producer_epoch()));
        assert_eq!(
            a.2.get(dendro::keys::PRODUCER_EPOCH).map(String::as_str),
            Some(metriken::epoch::producer_epoch()),
            "the epoch rides in the source metadata too"
        );
        assert!(!a.3, "a running producer's source has not ended");
    }

    #[test]
    fn the_handshake_is_sent_once_and_names_the_source() {
        let mut p = producer();
        assert!(!p.has_sent_handshake());
        let Frame::Handshake {
            clock_anchor_wall_ns,
            uuid,
            ..
        } = p.handshake()
        else {
            panic!("a handshake")
        };
        assert!(p.has_sent_handshake());
        assert_eq!(clock_anchor_wall_ns, 1_700_000_000_000_000_000);
        assert_eq!(uuid.as_deref(), Some(UUID));
    }

    /// Frames through dendro's own codec: what is encoded decodes to the
    /// same frames, and the payload to the same row.
    #[test]
    fn frames_round_trip_through_dendros_wire() {
        let mut p = producer();
        let mut bytes = p.opening().unwrap();
        let sent = vec![
            p.interval([&row(2)], 2_000, 7, 1),
            p.empty_interval(2),
            p.interval([&row(2)], 4_000, 7, 3),
        ];
        for frame in &sent {
            dendro::replicate::wire::encode_frame(frame, &mut bytes).unwrap();
        }

        let mut reader = dendro::replicate::wire::FrameReader::new(std::io::Cursor::new(bytes))
            .expect("the preamble is dendro's");
        let mut received = Vec::new();
        while let Some(frame) = reader.next_frame().unwrap() {
            received.push(frame);
        }
        assert!(matches!(received[0], Frame::Handshake { .. }));
        assert_eq!(&received[1..], &sent[..]);

        let first = decode_wal_group_row(&rows_of(&received[1])[0].row).unwrap();
        assert_eq!(first.schema, Some(schema(2)));
        assert_eq!(first.counters, vec![Some(0), Some(1)]);
        assert_eq!(first.window, Some((1_000, 2_000)));
    }

    /// Frames applied by dendro's subscriber land in an archive whose source
    /// is the producer's: its uuid, and rows naming no index state all
    /// resolve.
    #[cfg(feature = "write")]
    #[test]
    fn a_tick_survives_the_wire_and_lands_in_an_archive() {
        use dendro::replicate::wire::{self, FrameReader};
        use dendro::replicate::Subscriber;
        use dendro::segment::SegmentEncoder;
        use dendro::writer::Writer;

        struct NoSegments;
        impl SegmentEncoder for NoSegments {
            fn encode(&self, _: &str, _: &[WalRow]) -> dendro::segment::EncodeResult {
                Ok(None)
            }
            fn version(&self) -> Option<&str> {
                Some("frame-producer-test")
            }
        }

        let mut p = producer();
        let sent = vec![p.handshake(), p.interval([&row(3)], 2_000, 0, 7)];
        let mut stream = Vec::new();
        wire::write_preamble(&mut stream).unwrap();
        for frame in &sent {
            wire::encode_frame(frame, &mut stream).unwrap();
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subscribed.dendro");
        let mut subscriber = Subscriber::new(Writer::create(&path, Box::new(NoSegments)).unwrap());
        let mut reader = FrameReader::new(std::io::Cursor::new(stream)).unwrap();
        let (mut applied_rows, mut skipped) = (0usize, 0usize);
        while let Some(frame) = reader.next_frame().unwrap() {
            let applied = subscriber.apply(frame).unwrap();
            applied_rows += applied.rows;
            skipped += applied.rows_skipped;
        }
        drop(subscriber);

        assert_eq!(skipped, 0, "rows naming NO_INDEX_STATE always resolve");
        assert_eq!(applied_rows, 1);

        let archive = dendro::archive::Archive::open(&path).unwrap();
        let sources = archive.read_sources().unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].uuid.as_deref(), Some(UUID));
        let stored = archive
            .read_caller_rows(sources[0].id, STREAM, i64::MIN, i64::MAX)
            .unwrap();
        assert!(stored.is_empty(), "no index, so no caller rows");
    }
}
