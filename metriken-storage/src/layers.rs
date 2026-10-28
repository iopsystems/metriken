//! A source's identity in three layers, stored in its dendro metadata.
//!
//! Each layer is a JSON document under its own metadata key
//! ([`Layer::key`]): a list of entries, appended to and never rewritten, and a
//! cache of their fold. The source's `labels`, and the metadata keys the
//! layers set, are the layers merged in [`Layer::ORDER`], the last winning.
//! [`append`] writes them and keeps the layers, their caches and the merged
//! result in agreement. A label or layered metadata key changed by a write
//! that did not go through [`append`] (dendro's `update_source_labels` or
//! `update_source_metadata`) is folded in as a user entry by the next
//! [`append`] that appends an entry.
//!
//! Design: `docs/journal/2026-10-19-recording-identity-and-views.md`, Q2.
//!
//! [`append`] edits a source's two maps and does no I/O. It builds without the
//! `write` feature, so the WASM viewer can call it on maps read from an
//! in-memory archive. A recording applies its result to a live source with
//! dendro's `SourceWriter::set_identity`, which replaces both maps, keeping
//! only `encoder` and `writer_sessions`: the maps passed in must be the
//! source's current ones, `events` and earlier metadata patches included, or
//! the write erases them.

use std::collections::BTreeMap;
use std::fmt;

use metriken_model::identity::ProducerIdentity;
use serde::{Deserialize, Serialize};

/// The prefix of every metadata key metriken reserves, the layer keys among
/// them. An entry may not set or unset a label or metadata key under it
/// ([`append`] refuses it).
pub const RESERVED_PREFIX: &str = "metriken.";

/// The metadata key of the producer layer.
pub const PRODUCER_KEY: &str = "metriken.layer.producer";
/// The metadata key of the recorder layer.
pub const RECORDER_KEY: &str = "metriken.layer.recorder";
/// The metadata key of the user layer.
pub const USER_KEY: &str = "metriken.layer.user";

/// Metadata keys whose values are JSON objects merged key by key across
/// entries and layers, rather than replaced. rezolus's `descriptions` maps
/// metric names to descriptions, and a restarted agent that reports fewer
/// metrics still describes the ones it reported before.
pub const UNION_KEYS: &[&str] = &["descriptions"];

/// The producer layer's metadata key for a producer identity's name.
pub const PRODUCER_NAME: &str = "producer_name";
/// The producer layer's metadata key for a producer identity's version.
pub const VERSION: &str = "version";
/// The producer layer's metadata key for a producer identity's build.
pub const BUILD: &str = "build";
/// The producer layer's label for a producer identity's host.
pub const HOST: &str = "host";

/// Add `identity` to a producer entry's maps: [`PRODUCER_NAME`],
/// [`VERSION`] and [`BUILD`] to `metadata` and [`HOST`] to `labels`, each
/// that is set, replacing what the maps held under those keys.
pub fn identity_keys(
    identity: &ProducerIdentity,
    labels: &mut BTreeMap<String, String>,
    metadata: &mut BTreeMap<String, String>,
) {
    metadata.insert(PRODUCER_NAME.to_string(), identity.name.clone());
    for (key, value) in [(VERSION, &identity.version), (BUILD, &identity.build)] {
        if let Some(value) = value {
            metadata.insert(key.to_string(), value.clone());
        }
    }
    if let Some(host) = &identity.host {
        labels.insert(HOST.to_string(), host.clone());
    }
}

/// The flat epoch keys of a source recorded before the layers, which the
/// recorder layer's entries replace. An entry may not set them.
const PRODUCER_EPOCH: &str = "producer_epoch";
const PRODUCER_EPOCHS: &str = "producer_epochs";

/// Metadata keys dendro maintains for a source. They are not layered: a
/// derivation leaves them where they are, and an entry may not set or unset
/// them.
const UNLAYERED: &[&str] = &["writer_sessions", "encoder", "events"];

/// Whether an entry may not set or unset metadata key `key`: one under
/// [`RESERVED_PREFIX`], a flat epoch key (`producer_epoch`,
/// `producer_epochs`), or a key dendro maintains (`writer_sessions`,
/// `encoder`, `events`). A label is forbidden only under
/// [`RESERVED_PREFIX`]. A writer filters what it did not choose (a
/// producer's handshake) with this before building an entry, since
/// [`append`] refuses the whole entry.
pub fn forbidden_metadata(key: &str) -> bool {
    key.starts_with(RESERVED_PREFIX)
        || key == PRODUCER_EPOCH
        || key == PRODUCER_EPOCHS
        || UNLAYERED.contains(&key)
}

/// One of a source's three identity layers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Layer {
    /// What the recorder observed: the endpoint, the transport, each producer
    /// epoch, and what a probe hook read.
    Recorder,
    /// What the producer declared: its handshake, or an in-process source's
    /// identity.
    Producer,
    /// What a person asserted: record-time labels, `annotate`, the viewer.
    User,
}

impl Layer {
    /// The merge order, from least to most specific: a key set by a later
    /// layer wins.
    pub const ORDER: [Layer; 3] = [Layer::Recorder, Layer::Producer, Layer::User];

    /// The metadata key this layer is stored under.
    pub fn key(self) -> &'static str {
        match self {
            Layer::Recorder => RECORDER_KEY,
            Layer::Producer => PRODUCER_KEY,
            Layer::User => USER_KEY,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Layer::Recorder => "recorder",
            Layer::Producer => "producer",
            Layer::User => "user",
        }
    }
}

/// One write to a layer.
///
/// Fields this build does not know are kept in `extra` and written back, so
/// an older reader that appends to a newer file does not drop them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Entry {
    /// Set by the caller, normally a random UUID. [`append`] gives an entry it
    /// records for a change made outside it the id `external-<seq>-<id>`,
    /// where `<id>` is the appended entry's. An entry
    /// [derived](Layers::derive_legacy) from a source without layers has an
    /// id built from the source's uuid, the layer, the epoch and its
    /// `from_ts` (and the name, for the evidence's `producer_name`), so every
    /// derivation of one source gives the same ids; a legacy list that repeats
    /// an epoch at one `from_ts` gives two entries with one id.
    pub id: String,
    /// Append order within the layer, assigned by [`append`]: one more than
    /// the layer's highest, or 0 in an empty layer. A derived entry's is set
    /// by [`Layers::derive_legacy`].
    #[serde(default)]
    pub seq: u64,
    /// Wall time of the write, nanoseconds since the Unix epoch. Shown, never
    /// used for ordering. Absent on a derived entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<u64>,
    /// Who wrote it: `producer`, `recorder`, `record`, `annotate`, `viewer`,
    /// `upgrade`, `convert`, `combine`, or `external` for a change [`append`]
    /// found made outside it.
    pub by: String,
    /// The timestamp of the first row the entry covers, in the source's row
    /// unit. Producer and recorder entries only. Absent on an entry written
    /// before the source's first row; such an entry folds before every entry
    /// that has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_ts: Option<u64>,
    /// The producer epoch the entry belongs to. On a recorder entry it marks
    /// where that epoch's span starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<String>,
    /// Labels the entry sets.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// Metadata the entry sets.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    /// Labels the entry removes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unset_labels: Vec<String>,
    /// Metadata keys the entry removes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unset_metadata: Vec<String>,
    /// Fields this build does not know.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Entry {
    /// An entry with `id` and `by` and nothing set.
    pub fn new(id: impl Into<String>, by: impl Into<String>) -> Self {
        Entry {
            id: id.into(),
            by: by.into(),
            ..Entry::default()
        }
    }

    /// Whether two entries say the same thing: the same keys set and unset,
    /// in the same epoch. `id`, `seq`, `ts`, `by` and `from_ts` are not
    /// compared, so a handshake resent at a reconnect is the same entry as
    /// the one it repeats.
    pub fn same_contents(&self, other: &Entry) -> bool {
        self.labels == other.labels
            && self.metadata == other.metadata
            && self.unset_labels == other.unset_labels
            && self.unset_metadata == other.unset_metadata
            && self.epoch == other.epoch
    }

    /// The first key the entry sets or unsets that an entry may not touch: a
    /// label or metadata key under [`RESERVED_PREFIX`], a flat epoch key, or
    /// a key dendro maintains.
    fn forbidden_key(&self) -> Option<&str> {
        let labels = self.labels.keys().chain(&self.unset_labels);
        let metadata = self.metadata.keys().chain(&self.unset_metadata);
        labels
            .map(String::as_str)
            .find(|k| k.starts_with(RESERVED_PREFIX))
            .or_else(|| metadata.map(String::as_str).find(|k| forbidden_metadata(k)))
    }
}

/// One layer as stored: its entries and the cache of their fold.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LayerDoc {
    /// The fold's labels, as last written by [`append`].
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// The fold's metadata, as last written by [`append`].
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    /// Every entry, in the order written.
    #[serde(default)]
    pub entries: Vec<Entry>,
    /// Fields this build does not know.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// A layer's labels and metadata, folded from its entries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Folded {
    /// The labels.
    pub labels: BTreeMap<String, String>,
    /// The metadata.
    pub metadata: BTreeMap<String, String>,
}

impl Folded {
    fn apply(&mut self, labels: &BTreeMap<String, String>, metadata: &BTreeMap<String, String>) {
        for (k, v) in labels {
            self.labels.insert(k.clone(), v.clone());
        }
        for (k, v) in metadata {
            let merged = match self.metadata.get(k) {
                Some(old) if UNION_KEYS.contains(&k.as_str()) => union_objects(old, v),
                _ => None,
            };
            self.metadata
                .insert(k.clone(), merged.unwrap_or_else(|| v.clone()));
        }
    }

    fn apply_entry(&mut self, entry: &Entry) {
        for key in &entry.unset_labels {
            self.labels.remove(key);
        }
        for key in &entry.unset_metadata {
            self.metadata.remove(key);
        }
        self.apply(&entry.labels, &entry.metadata);
    }
}

/// `new`'s keys over `old`'s, when both are JSON objects; `None` otherwise,
/// and the caller takes `new` whole.
fn union_objects(old: &str, new: &str) -> Option<String> {
    let serde_json::Value::Object(mut merged) = serde_json::from_str(old).ok()? else {
        return None;
    };
    let serde_json::Value::Object(newer) = serde_json::from_str(new).ok()? else {
        return None;
    };
    merged.extend(newer);
    Some(serde_json::Value::Object(merged).to_string())
}

impl LayerDoc {
    /// The entries in fold order. The producer and recorder layers fold in
    /// (`from_ts`, `seq`, `id`) order, an absent `from_ts` first: restart stamps
    /// come from clocks that need not agree, so append order is not time
    /// order. The user layer folds in (`seq`, `id`) order, so a later edit
    /// wins whatever the writer's clock said.
    pub fn ordered(&self, layer: Layer) -> Vec<&Entry> {
        let mut entries: Vec<&Entry> = self.entries.iter().collect();
        match layer {
            Layer::User => entries.sort_by(|a, b| (a.seq, &a.id).cmp(&(b.seq, &b.id))),
            Layer::Producer | Layer::Recorder => {
                entries.sort_by(|a, b| (a.from_ts, a.seq, &a.id).cmp(&(b.from_ts, b.seq, &b.id)))
            }
        }
        entries
    }

    /// The layer's labels and metadata: its entries applied in
    /// [`ordered`](Self::ordered) order, each key taking the last value set.
    /// A key a later entry omits keeps its earlier value until an entry
    /// unsets it.
    pub fn fold(&self, layer: Layer) -> Folded {
        let mut folded = Folded::default();
        for entry in self.ordered(layer) {
            folded.apply_entry(entry);
        }
        folded
    }

    /// One more than the highest `seq`, 0 for no entries; an error when the
    /// highest is `u64::MAX`.
    fn next_seq(&self, layer: Layer) -> Result<u64, LayerError> {
        match self.entries.iter().map(|e| e.seq).max() {
            None => Ok(0),
            Some(s) => s.checked_add(1).ok_or_else(|| LayerError {
                key: layer.key().to_string(),
                message: format!("an entry's seq is {s}, so no later entry can be appended"),
            }),
        }
    }
}

/// A layer key that does not parse, or an entry [`append`] refuses.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LayerError {
    /// The metadata key the error is about.
    pub key: String,
    /// What went wrong.
    pub message: String,
}

impl fmt::Display for LayerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "source metadata {}: {}", self.key, self.message)
    }
}

impl std::error::Error for LayerError {}

/// What a source recorded before the layers is known to be, from evidence
/// its reader holds (the journal's D2), for [`Layers::derive_legacy`] to
/// write as `producer_name` in an at-open recorder entry of its own.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct LegacyEvidence {
    /// The producer the evidence names, written as `producer_name`.
    pub producer_name: Option<String>,
}

/// A source's three layers.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct Layers {
    /// The recorder layer.
    pub recorder: LayerDoc,
    /// The producer layer.
    pub producer: LayerDoc,
    /// The user layer.
    pub user: LayerDoc,
}

impl Layers {
    /// The layers stored in `metadata`, or `None` for a source that has none
    /// of the layer keys (one recorded before them). A key that is present
    /// and does not parse is an error rather than an empty layer, so a write
    /// cannot replace a damaged layer with a short one.
    pub fn read(metadata: &BTreeMap<String, String>) -> Result<Option<Layers>, LayerError> {
        if !Layer::ORDER.iter().any(|l| metadata.contains_key(l.key())) {
            return Ok(None);
        }
        let mut layers = Layers::default();
        for layer in Layer::ORDER {
            if let Some(doc) = read_layer(metadata, layer)? {
                *layers.get_mut(layer) = doc;
            }
        }
        Ok(Some(layers))
    }

    /// One layer.
    pub fn get(&self, layer: Layer) -> &LayerDoc {
        match layer {
            Layer::Recorder => &self.recorder,
            Layer::Producer => &self.producer,
            Layer::User => &self.user,
        }
    }

    fn get_mut(&mut self, layer: Layer) -> &mut LayerDoc {
        match layer {
            Layer::Recorder => &mut self.recorder,
            Layer::Producer => &mut self.producer,
            Layer::User => &mut self.user,
        }
    }

    /// The layers merged in [`Layer::ORDER`], each folded from its entries:
    /// what the source's `labels` and layered metadata keys should hold.
    ///
    /// A user entry removes a key a lower layer sets by setting it to `""`,
    /// as Prometheus treats an empty label value as no label; a key whose
    /// merged value is empty is left out. In the recorder and producer layers
    /// an empty value removes the key from that layer, as an unset does; it
    /// does not remove a lower layer's value. Unsetting a key removes it from the
    /// entry's layer, so the merged result takes the lower layers' value; to
    /// undo one edit, an entry writes the key's earlier value.
    pub fn merged(&self) -> Folded {
        let mut merged = self.merged_with_removals();
        merged.labels.retain(|_, v| !v.is_empty());
        merged.metadata.retain(|_, v| !v.is_empty());
        merged
    }

    /// [`merged`](Self::merged) before the user layer's empty values are
    /// left out: an empty value is a key the user layer removed.
    fn merged_with_removals(&self) -> Folded {
        let mut merged = Folded::default();
        for layer in Layer::ORDER {
            let mut folded = self.get(layer).fold(layer);
            if layer != Layer::User {
                folded.labels.retain(|_, v| !v.is_empty());
                folded.metadata.retain(|_, v| !v.is_empty());
            }
            merged.apply(&folded.labels, &folded.metadata);
        }
        merged
    }

    /// The layers a source recorded before them is read as. Every entry has
    /// no `ts`, has `by` set to `upgrade`, and has an id built from `uuid`,
    /// the layer, and the epoch and `from_ts` it covers, so every derivation
    /// of one source gives the same entries:
    ///
    /// - its labels, other than those under [`RESERVED_PREFIX`], as one user
    ///   entry, `seq` 0;
    /// - an at-open recorder entry (no epoch, no `from_ts`, `seq` 0) holding
    ///   its metadata, other than the keys dendro maintains
    ///   (`writer_sessions`, `encoder`, `events`), the flat epoch keys and
    ///   reserved keys. It folds below every later recorder entry, and the
    ///   recorder layer below the producer and user layers, so a value a
    ///   later producer or user entry sets and a still later one in that
    ///   layer unsets falls back to the source's own. A recorder entry that
    ///   unsets a key removes the source's own value too, since an unset
    ///   removes the key from its whole layer;
    /// - the producer `evidence` names, as `producer_name` in a further
    ///   at-open recorder entry with `seq` 1, which folds over the at-open
    ///   entry, and an id built from the name. Readers holding different
    ///   evidence derive the same entries apart from this one;
    /// - a producer identity its metadata declares under
    ///   `metriken.producer.*` (a producer that declared one, recorded before
    ///   the recorder read it), as one producer entry, `seq` 0, written as
    ///   [`identity_keys`] writes it. Above the at-open recorder entry, so its
    ///   `version` replaces a flat `version` the source carried;
    /// - each epoch, as a recorder entry carrying that `epoch` and its
    ///   `from_ts`, read as [`epoch_spans`](crate::process::epoch_spans)
    ///   reads the flat keys: the `producer_epochs` list when it parses and
    ///   is not empty, an entry repeated in it once per listing, else
    ///   `producer_epoch` with no `from_ts`. Its `seq` is its position in the
    ///   list, so epochs listed at one `from_ts` fold, and give spans, in list
    ///   order.
    ///
    /// A listed epoch's `version` is not derived: the source's `version` is
    /// its newest, which the at-open entry holds, and the flat
    /// `producer_epochs`, which no write removes, keeps the history.
    pub fn derive_legacy(
        uuid: Option<&str>,
        labels: &BTreeMap<String, String>,
        metadata: &BTreeMap<String, String>,
        evidence: &LegacyEvidence,
    ) -> Layers {
        let mut layers = Layers::default();
        let labels: BTreeMap<String, String> = labels
            .iter()
            .filter(|(k, _)| !k.starts_with(RESERVED_PREFIX))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if !labels.is_empty() {
            let mut entry = Entry::new(derived_id(uuid, Layer::User, None, None), "upgrade");
            entry.labels = labels;
            layers.user.entries.push(entry);
        }

        let mut opened = Entry::new(derived_id(uuid, Layer::Recorder, None, None), "upgrade");
        opened.metadata = metadata
            .iter()
            .filter(|(k, _)| {
                !UNLAYERED.contains(&k.as_str())
                    && !k.starts_with(RESERVED_PREFIX)
                    && *k != PRODUCER_EPOCH
                    && *k != PRODUCER_EPOCHS
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if !opened.metadata.is_empty() {
            layers.recorder.entries.push(opened);
        }
        if let Some(identity) = ProducerIdentity::from_metadata(metadata) {
            let mut declared = Entry::new(derived_id(uuid, Layer::Producer, None, None), "upgrade");
            identity_keys(&identity, &mut declared.labels, &mut declared.metadata);
            layers.producer.entries.push(declared);
        }
        if let Some(name) = &evidence.producer_name {
            let tag = format!("producer_name:{name}");
            let mut named = Entry::new(
                derived_id(uuid, Layer::Recorder, Some(&tag), None),
                "upgrade",
            );
            named.seq = 1;
            named.metadata = BTreeMap::from([("producer_name".to_string(), name.clone())]);
            layers.recorder.entries.push(named);
        }

        for (position, listed) in legacy_epochs(metadata).into_iter().enumerate() {
            let id = derived_id(uuid, Layer::Recorder, Some(&listed.epoch), listed.from_ts);
            let mut entry = Entry::new(id, "upgrade");
            entry.seq = position as u64;
            entry.from_ts = listed.from_ts;
            entry.epoch = Some(listed.epoch);
            layers.recorder.entries.push(entry);
        }
        for layer in Layer::ORDER {
            refresh_cache(layers.get_mut(layer), layer);
        }
        layers
    }
}

/// One layer from `metadata`, `None` when its key is absent.
fn read_layer(
    metadata: &BTreeMap<String, String>,
    layer: Layer,
) -> Result<Option<LayerDoc>, LayerError> {
    let Some(raw) = metadata.get(layer.key()) else {
        return Ok(None);
    };
    serde_json::from_str(raw).map(Some).map_err(|e| LayerError {
        key: layer.key().to_string(),
        message: e.to_string(),
    })
}

/// The recorder layer alone, for a reader that needs only it, so a damaged
/// user or producer layer does not hide it. `None` when the key is absent.
pub fn read_recorder(metadata: &BTreeMap<String, String>) -> Result<Option<LayerDoc>, LayerError> {
    read_layer(metadata, Layer::Recorder)
}

/// The id of an entry [derived](Layers::derive_legacy) for `layer`, `epoch`
/// and `from_ts` of the source with `uuid`.
fn derived_id(
    uuid: Option<&str>,
    layer: Layer,
    epoch: Option<&str>,
    from_ts: Option<u64>,
) -> String {
    // Each field as a presence byte, then its length and bytes, so an absent
    // value and an empty one differ and no two field lists share an input.
    fn field(input: &mut Vec<u8>, value: Option<&[u8]>) {
        match value {
            None => input.push(0),
            Some(bytes) => {
                input.push(1);
                input.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
                input.extend_from_slice(bytes);
            }
        }
    }
    let mut input = Vec::new();
    field(&mut input, uuid.map(str::as_bytes));
    field(&mut input, Some(layer.name().as_bytes()));
    field(&mut input, epoch.map(str::as_bytes));
    let from_ts = from_ts.map(u64::to_le_bytes);
    field(&mut input, from_ts.as_ref().map(|b| &b[..]));
    let hash = twox_hash::XxHash3_128::oneshot(&input);
    format!("derived-{hash:032x}")
}

/// One epoch of a source recorded before the layers.
struct LegacyEpoch {
    epoch: String,
    from_ts: Option<u64>,
}

/// The epochs of a source recorded before the layers, as `epoch_spans`
/// reads them: the `producer_epochs` list when it parses and is not empty,
/// in list order and each listing kept, else `producer_epoch` alone.
fn legacy_epochs(metadata: &BTreeMap<String, String>) -> Vec<LegacyEpoch> {
    let listed = metadata
        .get(PRODUCER_EPOCHS)
        .and_then(|raw| crate::process::listed_epochs(raw))
        .filter(|l| !l.is_empty());
    match listed {
        Some(listed) => listed
            .into_iter()
            .map(|(epoch, from_ts)| LegacyEpoch {
                epoch,
                from_ts: Some(from_ts),
            })
            .collect(),
        None => match metadata.get(PRODUCER_EPOCH) {
            Some(epoch) if !epoch.is_empty() => vec![LegacyEpoch {
                epoch: epoch.clone(),
                from_ts: None,
            }],
            _ => Vec::new(),
        },
    }
}

fn refresh_cache(doc: &mut LayerDoc, layer: Layer) {
    let folded = doc.fold(layer);
    doc.labels = folded.labels;
    doc.metadata = folded.metadata;
}

/// A user entry for what changed in `labels` and `metadata` since the
/// layers' merged result `before` (with the user layer's removals) was
/// written: labels added, changed or removed, and layered metadata keys
/// changed, added back or removed. An empty value and an absent key are the
/// same. A removed key is set to `""`, which removes it from the merged
/// result whichever layer sets it. A label under [`RESERVED_PREFIX`] is not
/// taken, so `append`, which writes the merged labels whole, drops it.
/// `None` when nothing changed.
fn drift(
    before: &Folded,
    labels: &BTreeMap<String, String>,
    metadata: &BTreeMap<String, String>,
) -> Option<Entry> {
    fn present<'a>(map: &'a BTreeMap<String, String>, k: &str) -> Option<&'a String> {
        map.get(k).filter(|v| !v.is_empty())
    }
    let mut entry = Entry::new("", "external");
    for k in before.labels.keys().chain(labels.keys()) {
        if k.starts_with(RESERVED_PREFIX) {
            continue;
        }
        let have = present(labels, k);
        if present(&before.labels, k) != have {
            entry
                .labels
                .insert(k.clone(), have.cloned().unwrap_or_default());
        }
    }
    for k in before.metadata.keys() {
        let have = present(metadata, k);
        let was = present(&before.metadata, k);
        if was != have && !union_holds(k, was, have) {
            let value =
                union_added(k, was, have).unwrap_or_else(|| have.cloned().unwrap_or_default());
            entry.metadata.insert(k.clone(), value);
        }
    }
    let changed = !entry.labels.is_empty() || !entry.metadata.is_empty();
    changed.then_some(entry)
}

/// For union key `k`, the entries of `have` (the value found in the source)
/// that `was` (the merged value) lacks or holds with another value, as a
/// JSON object; `None` for another key or when either is not an object. An
/// `external` entry then carries only what changed, so a later producer or
/// recorder value of another name in the object is not hidden under it.
fn union_added(k: &str, was: Option<&String>, have: Option<&String>) -> Option<String> {
    use serde_json::Value;
    if !UNION_KEYS.contains(&k) {
        return None;
    }
    let parse = |s: &str| match serde_json::from_str::<Value>(s) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    };
    let (was, have) = (parse(was?)?, parse(have?)?);
    let added: serde_json::Map<String, Value> = have
        .into_iter()
        .filter(|(name, value)| was.get(name) != Some(value))
        .collect();
    Some(Value::Object(added).to_string())
}

/// Whether `have`, the value of union key `k` found in the source, adds
/// nothing to `was`, the merged value: both JSON objects and every key of
/// `have` in `was` with the same value. Such a value is not a change, since
/// the merge would give `was` back.
fn union_holds(k: &str, was: Option<&String>, have: Option<&String>) -> bool {
    let (Some(was), Some(have)) = (was, have) else {
        return false;
    };
    if !UNION_KEYS.contains(&k) {
        return false;
    }
    let parse = |s: &str| serde_json::from_str::<serde_json::Value>(s).ok();
    match (
        union_objects(was, have).as_deref().and_then(parse),
        parse(was),
    ) {
        (Some(merged), Some(was)) => merged == was,
        _ => false,
    }
}

/// Append `entry` to `layer` of a source, and rewrite what the layers
/// determine: the layers' keys in `metadata`, the source's `labels`, and the
/// metadata keys the layers set. Returns whether anything was appended.
///
/// `seq` is assigned here; the caller sets `id`, `ts`, `by` and, for a
/// producer or recorder entry, `from_ts` and `epoch`. An entry that sets or
/// unsets a label or metadata key under [`RESERVED_PREFIX`], a flat epoch
/// key, or `writer_sessions`, `encoder` or `events` is refused. A producer
/// entry with the same contents as the producer entry that wins the fold
/// appends nothing; so does one with the same contents as the producer entry
/// appended last when appending it would change neither the layer's fold nor
/// the fold of the layer's entries in the entry's epoch. A skipped entry is
/// not in the layer, so a later entry whose `from_ts` falls between the
/// copy on record and the skipped one folds after the copy and its values
/// win, where the full sequence of handshakes would have put the skipped
/// one after it and the skipped one's values would win. Only the producer
/// layer is deduplicated this way.
///
/// A source with no layer keys is first given the layers
/// [`Layers::derive_legacy`] reads from it, with `uuid` and `evidence`, so
/// its record-time labels and metadata enter the fold before the entry.
///
/// A source whose labels, or whose metadata keys the layers set, were changed
/// since the last write through this function (by dendro's
/// `update_source_labels` or `update_source_metadata`) has that change appended
/// first, as a user entry by `external` with the id `external-<seq>-<entry's
/// id>`, so it is kept rather than overwritten; a removal is kept as the key
/// set to `""`. Being a user entry, it wins over every producer and recorder
/// value of that key, later ones included, until a user entry unsets it. A
/// `descriptions` value whose keys all hold their merged values (one with a key
/// removed, or reordered) is not a change: that key is merged across entries,
/// so a removed key comes back on the next write. Any other change to it is
/// recorded as the names it adds or gives another value, so the entry pins only
/// those. The write that derives a legacy source's layers checks nothing, since
/// the derivation is the source's own state.
///
/// `labels` becomes the merged labels whole: every label of the source is in
/// a layer. In `metadata`, a key the merged layers set is written, and a key
/// they set before this write and no longer set is removed; keys the layers
/// never set (dendro's `writer_sessions`, a legacy `producer_epochs`) are left
/// alone.
///
/// # Errors
///
/// A refused entry, and a layer key (`metriken.layer.*`) in `metadata` that
/// does not parse. The second fails this and every later `append` on the
/// source until the key is repaired or removed outside `append`; an entry
/// cannot unset it. An `append` that must add an entry to a layer whose
/// highest `seq` is `u64::MAX`, the `external` entry included, fails.
/// Nothing is changed on an error.
pub fn append(
    labels: &mut BTreeMap<String, String>,
    metadata: &mut BTreeMap<String, String>,
    uuid: Option<&str>,
    evidence: &LegacyEvidence,
    layer: Layer,
    mut entry: Entry,
) -> Result<bool, LayerError> {
    if let Some(key) = entry.forbidden_key() {
        return Err(LayerError {
            key: key.to_string(),
            message: format!(
                "an entry may not set or unset {key}: the {RESERVED_PREFIX} keys are written by \
                 layers::append, the flat epoch keys are the recorder layer's epochs, and \
                 writer_sessions, encoder and events are dendro's"
            ),
        });
    }
    let (mut layers, derived) = match Layers::read(metadata)? {
        Some(layers) => (layers, false),
        None => (
            Layers::derive_legacy(uuid, labels, metadata, evidence),
            true,
        ),
    };
    let before = layers.merged();

    if !derived {
        if let Some(mut external) = drift(&layers.merged_with_removals(), labels, metadata) {
            let user = layers.get_mut(Layer::User);
            external.seq = user.next_seq(Layer::User)?;
            external.id = format!("external-{}-{}", external.seq, entry.id);
            external.ts = entry.ts;
            user.entries.push(external);
        }
    }

    let doc = layers.get_mut(layer);
    // Before the dedupe, whose fold check places the entry by its `seq`.
    entry.seq = doc.next_seq(layer)?;
    if layer == Layer::Producer {
        let winner = doc.ordered(layer).last().copied();
        let newest = doc.entries.iter().max_by_key(|e| e.seq);
        let repeats_winner = winner.is_some_and(|w| w.same_contents(&entry));
        // Whether appending leaves the fold of the entries `keep` selects
        // unchanged.
        let unchanged = |keep: &dyn Fn(&Entry) -> bool| {
            let mut without = doc.clone();
            without.entries.retain(|e| keep(e));
            let mut with = without.clone();
            with.entries.push(entry.clone());
            with.fold(layer) == without.fold(layer)
        };
        let repeats_newest = newest.is_some_and(|n| n.same_contents(&entry))
            && unchanged(&|_| true)
            && unchanged(&|e| e.epoch == entry.epoch);
        if repeats_winner || repeats_newest {
            return Ok(false);
        }
    }
    doc.entries.push(entry);

    for layer in Layer::ORDER {
        refresh_cache(layers.get_mut(layer), layer);
    }
    let after = layers.merged();

    *labels = after.labels;
    for key in before.metadata.keys() {
        if !after.metadata.contains_key(key) {
            metadata.remove(key);
        }
    }
    // A key the user layer empties goes even when no layer set it (one
    // written outside `append`); `forbidden_key` keeps dendro's keys out.
    for (key, value) in &layers.merged_with_removals().metadata {
        if value.is_empty() {
            metadata.remove(key);
        }
    }
    metadata.extend(after.metadata);
    for layer in Layer::ORDER {
        let encoded =
            serde_json::to_string(layers.get(layer)).expect("a layer document always serializes");
        metadata.insert(layer.key().to_string(), encoded);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn entry(id: &str, by: &str, labels: &[(&str, &str)]) -> Entry {
        Entry {
            labels: map(labels),
            ..Entry::new(id, by)
        }
    }

    const NONE: LegacyEvidence = LegacyEvidence {
        producer_name: None,
    };

    fn put(
        labels: &mut BTreeMap<String, String>,
        metadata: &mut BTreeMap<String, String>,
        layer: Layer,
        e: Entry,
    ) -> bool {
        append(labels, metadata, Some("u"), &NONE, layer, e).unwrap()
    }

    /// The first write to a source without layers keeps its labels: they
    /// become a derived user entry, folded under the new one.
    #[test]
    fn first_write_derives_the_legacy_labels() {
        let mut labels = map(&[("source", "rezolus"), ("arm", "baseline")]);
        let mut metadata = map(&[("writer_sessions", "[]")]);
        assert!(put(
            &mut labels,
            &mut metadata,
            Layer::User,
            entry("e-1", "annotate", &[("tier", "server")]),
        ));
        assert_eq!(
            labels,
            map(&[
                ("source", "rezolus"),
                ("arm", "baseline"),
                ("tier", "server")
            ])
        );
        let layers = Layers::read(&metadata).unwrap().unwrap();
        let user = &layers.user.entries;
        assert_eq!(user.len(), 2);
        assert_eq!(
            (user[0].seq, user[0].by.as_str(), user[0].ts),
            (0, "upgrade", None)
        );
        assert_eq!(user[1].seq, 1);
        assert_eq!(
            metadata.get("writer_sessions").map(String::as_str),
            Some("[]")
        );
        assert!(
            layers.recorder.entries.is_empty(),
            "writer_sessions is not layered"
        );
    }

    /// A legacy metadata value overridden and then unset falls back to the
    /// source's own, which the derived at-open recorder entry holds.
    #[test]
    fn legacy_metadata_survives_an_override_and_its_undo() {
        let mut labels = BTreeMap::new();
        let mut metadata = map(&[("service_queries", "ORIGINAL"), ("encoder", "1")]);
        let mut set = Entry::new("a", "annotate");
        set.metadata = map(&[("service_queries", "NEW")]);
        put(&mut labels, &mut metadata, Layer::User, set);
        assert_eq!(metadata["service_queries"], "NEW");
        let mut undo = Entry::new("b", "annotate");
        undo.unset_metadata = vec!["service_queries".to_string()];
        put(&mut labels, &mut metadata, Layer::User, undo);
        assert_eq!(metadata["service_queries"], "ORIGINAL");
        assert_eq!(metadata["encoder"], "1");
    }

    /// The producer the caller's evidence names goes into an at-open recorder
    /// entry of its own, whose id is built from the name.
    #[test]
    fn evidence_names_the_producer_in_the_derivation() {
        let evidence = LegacyEvidence {
            producer_name: Some("rezolus".to_string()),
        };
        let a = Layers::derive_legacy(Some("u"), &BTreeMap::new(), &BTreeMap::new(), &evidence);
        let named = &a.recorder.entries[0];
        assert_eq!(named.metadata["producer_name"], "rezolus");
        assert_eq!((named.epoch.as_ref(), named.from_ts), (None, None));
        // Metadata with no `producer_name` derives the same at-open entry
        // whatever the evidence; the evidence is an entry of its own.
        let own = map(&[("k", "v")]);
        let with = Layers::derive_legacy(Some("u"), &BTreeMap::new(), &own, &evidence);
        let without = Layers::derive_legacy(Some("u"), &BTreeMap::new(), &own, &NONE);
        assert_eq!(with.recorder.entries[0], without.recorder.entries[0]);
        assert_eq!(with.recorder.entries.len(), 2);
        assert_eq!(with.recorder.entries[1].id, named.id);
        assert_ne!(named.id, with.recorder.entries[0].id);
    }

    /// Two derivations of one source give the same ids, and two sources
    /// different ones; a repeated epoch keeps both listings.
    #[test]
    fn derived_ids_depend_on_uuid_layer_epoch_and_from_ts() {
        let labels = map(&[("source", "a")]);
        let metadata = map(&[(
            "producer_epochs",
            r#"[{"epoch":"A","from_ts":10},{"epoch":"B","from_ts":20},{"epoch":"A","from_ts":30,"version":"2"}]"#,
        )]);
        let a = Layers::derive_legacy(Some("u-1"), &labels, &metadata, &NONE);
        let b = Layers::derive_legacy(Some("u-1"), &labels, &BTreeMap::new(), &NONE);
        let c = Layers::derive_legacy(Some("u-2"), &labels, &metadata, &NONE);
        assert_eq!(a.user.entries[0].id, b.user.entries[0].id);
        assert_ne!(a.user.entries[0].id, c.user.entries[0].id);
        let ids: BTreeSet<&str> = a.recorder.entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids.len(), 3);
        assert_eq!(
            a.recorder
                .entries
                .iter()
                .map(|e| (e.epoch.as_deref(), e.from_ts))
                .collect::<Vec<_>>(),
            vec![
                (Some("A"), Some(10)),
                (Some("B"), Some(20)),
                (Some("A"), Some(30))
            ]
        );
        assert!(
            a.producer.entries.is_empty(),
            "a listed version is not derived"
        );
    }

    /// A legacy source with only `producer_epoch` keeps its epoch span after
    /// its first write, and the epoch derives with no `from_ts`.
    #[test]
    fn a_lone_producer_epoch_is_derived() {
        let mut labels = BTreeMap::new();
        let mut metadata = map(&[("producer_epoch", "e1")]);
        let span = Some((10, 20));
        let before = crate::process::epoch_spans(&metadata, span);
        put(
            &mut labels,
            &mut metadata,
            Layer::User,
            entry("x", "viewer", &[("k", "v")]),
        );
        assert_eq!(crate::process::epoch_spans(&metadata, span), before);
        assert_eq!(before.len(), 1);
    }

    /// A repeated epoch keeps its later span after the first write.
    #[test]
    fn a_repeated_epoch_keeps_its_spans() {
        let mut labels = BTreeMap::new();
        let mut metadata = map(&[(
            "producer_epochs",
            r#"[{"epoch":"A","from_ts":10},{"epoch":"B","from_ts":20},{"epoch":"A","from_ts":30}]"#,
        )]);
        let span = Some((10, 40));
        let before = crate::process::epoch_spans(&metadata, span);
        put(
            &mut labels,
            &mut metadata,
            Layer::User,
            entry("x", "viewer", &[("k", "v")]),
        );
        assert_eq!(crate::process::epoch_spans(&metadata, span), before);
        assert_eq!(before.len(), 3);
    }

    /// The user layer wins over the producer layer, which wins over the
    /// recorder layer.
    #[test]
    fn layers_merge_in_order() {
        let mut labels = BTreeMap::new();
        let mut metadata = BTreeMap::new();
        for (layer, value) in [
            (Layer::User, "user"),
            (Layer::Recorder, "recorder"),
            (Layer::Producer, "producer"),
        ] {
            let mut e = entry(value, value, &[("k", value), (value, "1")]);
            e.metadata = map(&[("m", value)]);
            put(&mut labels, &mut metadata, layer, e);
        }
        assert_eq!(labels.get("k").map(String::as_str), Some("user"));
        assert_eq!(metadata.get("m").map(String::as_str), Some("user"));
        assert_eq!(labels.len(), 4, "{labels:?}");
    }

    /// A later user edit wins by `seq`, whatever its `ts` says.
    #[test]
    fn user_edits_order_by_seq_not_ts() {
        let mut labels = BTreeMap::new();
        let mut metadata = BTreeMap::new();
        let mut first = entry("b", "viewer", &[("tier", "client")]);
        first.ts = Some(2_000);
        let mut second = entry("a", "viewer", &[("tier", "server")]);
        second.ts = Some(1_000);
        put(&mut labels, &mut metadata, Layer::User, first);
        put(&mut labels, &mut metadata, Layer::User, second);
        assert_eq!(labels.get("tier").map(String::as_str), Some("server"));
    }

    /// Producer and recorder entries fold by `from_ts`, so a restart stamped
    /// by a slow clock still loses to the later epoch.
    #[test]
    fn producer_and_recorder_entries_fold_by_from_ts() {
        for layer in [Layer::Producer, Layer::Recorder] {
            let mut doc = LayerDoc::default();
            let mut late = entry("late", "x", &[("version", "2")]);
            late.seq = 0;
            late.from_ts = Some(200);
            let mut early = entry("early", "x", &[("version", "1")]);
            early.seq = 1;
            early.from_ts = Some(100);
            doc.entries = vec![late, early];
            assert_eq!(
                doc.fold(layer).labels.get("version").map(String::as_str),
                Some("2"),
                "{layer:?}"
            );
        }
    }

    /// `unset_labels` and `unset_metadata` remove their own kind of key only,
    /// and a metadata key goes from the source too; a key no layer ever set
    /// is left alone.
    #[test]
    fn unset_removes_from_the_source() {
        let mut labels = BTreeMap::new();
        let mut metadata = map(&[("encoder", "1")]);
        let mut set = entry("s", "annotate", &[("zone", "a"), ("source", "x")]);
        set.metadata = map(&[("service_queries", "{}"), ("source", "x")]);
        put(&mut labels, &mut metadata, Layer::User, set);
        assert!(metadata.contains_key("service_queries"));

        let mut undo = Entry::new("u", "annotate");
        undo.unset_labels = vec!["zone".to_string(), "source".to_string()];
        undo.unset_metadata = vec!["service_queries".to_string()];
        put(&mut labels, &mut metadata, Layer::User, undo);
        assert!(!labels.contains_key("zone"));
        assert!(!labels.contains_key("source"));
        assert_eq!(metadata.get("source").map(String::as_str), Some("x"));
        assert!(!metadata.contains_key("service_queries"));
        assert_eq!(metadata.get("encoder").map(String::as_str), Some("1"));
    }

    /// A reconnect resending the handshake that wins the fold appends
    /// nothing, whatever its `from_ts`; a changed one appends.
    #[test]
    fn an_identical_handshake_appends_nothing() {
        let mut labels = BTreeMap::new();
        let mut metadata = BTreeMap::new();
        let hs = |id: &str, version: &str, from_ts: u64| {
            let mut e = entry(id, "producer", &[("source", "rezolus")]);
            e.metadata = map(&[("version", version)]);
            e.epoch = Some("e1".to_string());
            e.from_ts = Some(from_ts);
            e
        };
        assert!(put(
            &mut labels,
            &mut metadata,
            Layer::Producer,
            hs("1", "5", 10)
        ));
        assert!(!put(
            &mut labels,
            &mut metadata,
            Layer::Producer,
            hs("2", "5", 20)
        ));
        assert!(put(
            &mut labels,
            &mut metadata,
            Layer::Producer,
            hs("3", "6", 30)
        ));
        let layers = Layers::read(&metadata).unwrap().unwrap();
        assert_eq!(layers.producer.entries.len(), 2);
    }

    /// The dedupe compares with the entry that wins the fold, not the one
    /// with the highest `seq`: an entry stamped earlier by a skewed clock but
    /// written later does not hide a resend of the winner.
    #[test]
    fn the_dedupe_compares_with_the_fold_winner() {
        let mut labels = BTreeMap::new();
        let mut metadata = BTreeMap::new();
        let hs = |id: &str, version: &str, from_ts: u64| {
            let mut e = Entry::new(id, "producer");
            e.metadata = map(&[("version", version)]);
            e.from_ts = Some(from_ts);
            e
        };
        put(
            &mut labels,
            &mut metadata,
            Layer::Producer,
            hs("v2", "2", 200),
        );
        put(
            &mut labels,
            &mut metadata,
            Layer::Producer,
            hs("v1", "1", 100),
        );
        assert_eq!(metadata["version"], "2");
        assert!(!put(
            &mut labels,
            &mut metadata,
            Layer::Producer,
            hs("v2b", "2", 300)
        ));
    }

    /// `descriptions` is unioned across entries and across layers.
    #[test]
    fn descriptions_union() {
        let mut labels = BTreeMap::new();
        let mut metadata = BTreeMap::new();
        for (layer, id, desc) in [
            (Layer::Recorder, "1", r#"{"a":"A","b":"B"}"#),
            (Layer::Recorder, "2", r#"{"b":"B2","c":"C"}"#),
            (Layer::Producer, "3", r#"{"d":"D"}"#),
        ] {
            let mut e = Entry::new(id, "x");
            e.metadata = map(&[("descriptions", desc)]);
            put(&mut labels, &mut metadata, layer, e);
        }
        let got: serde_json::Value = serde_json::from_str(&metadata["descriptions"]).unwrap();
        assert_eq!(
            got,
            serde_json::json!({"a": "A", "b": "B2", "c": "C", "d": "D"})
        );
    }

    /// A damaged layer key is an error, not an empty layer that the write
    /// would then store over it.
    #[test]
    fn a_damaged_layer_is_an_error() {
        let mut labels = BTreeMap::new();
        let mut metadata = map(&[(USER_KEY, "not json")]);
        let err = append(
            &mut labels,
            &mut metadata,
            None,
            &NONE,
            Layer::User,
            Entry::new("x", "viewer"),
        )
        .unwrap_err();
        assert_eq!(err.key, USER_KEY);
        assert_eq!(metadata[USER_KEY], "not json");
    }

    /// An entry may not write a reserved key or a flat epoch key.
    #[test]
    fn reserved_and_flat_epoch_keys_are_refused() {
        for key in [
            "metriken.layer.user",
            "metriken.other",
            "producer_epoch",
            "producer_epochs",
        ] {
            let mut e = Entry::new("x", "annotate");
            e.metadata = map(&[(key, "v")]);
            let err = append(
                &mut BTreeMap::new(),
                &mut BTreeMap::new(),
                None,
                &NONE,
                Layer::User,
                e,
            )
            .unwrap_err();
            assert_eq!(err.key, key);
        }
    }

    /// A label set outside `append` after the layers exist is kept, as a user
    /// entry by `external`, rather than dropped by the next write.
    #[test]
    fn a_change_made_outside_append_is_kept() {
        let mut labels = BTreeMap::new();
        let mut metadata = BTreeMap::new();
        put(
            &mut labels,
            &mut metadata,
            Layer::User,
            entry("a", "viewer", &[("k", "v")]),
        );
        labels.insert("arm".to_string(), "baseline".to_string());
        labels.remove("k");
        put(
            &mut labels,
            &mut metadata,
            Layer::Recorder,
            entry("b", "recorder", &[("r", "1")]),
        );
        assert_eq!(labels.get("arm").map(String::as_str), Some("baseline"));
        assert!(!labels.contains_key("k"));
        let layers = Layers::read(&metadata).unwrap().unwrap();
        let external = layers
            .user
            .entries
            .iter()
            .find(|e| e.by == "external")
            .unwrap();
        assert_eq!(external.id, "external-1-b");
        assert_eq!(external.labels.get("k").map(String::as_str), Some(""));
    }

    /// Fields this build does not know survive a write.
    #[test]
    fn unknown_fields_are_kept() {
        let mut labels = BTreeMap::new();
        let mut metadata = map(&[(
            USER_KEY,
            r#"{"labels":{},"metadata":{},"entries":[{"id":"f","by":"viewer","future":1}],"note":"x"}"#,
        )]);
        put(
            &mut labels,
            &mut metadata,
            Layer::User,
            entry("g", "viewer", &[("k", "v")]),
        );
        let doc: serde_json::Value = serde_json::from_str(&metadata[USER_KEY]).unwrap();
        assert_eq!(doc["note"], "x");
        assert_eq!(doc["entries"][0]["future"], 1);
    }

    /// The caches in each layer and the top-level maps match the fold after
    /// every write.
    #[test]
    fn caches_match_the_fold() {
        let mut labels = map(&[("source", "x")]);
        let mut metadata = BTreeMap::new();
        put(
            &mut labels,
            &mut metadata,
            Layer::Recorder,
            entry("r", "recorder", &[("instance", "http://h:4241")]),
        );
        let layers = Layers::read(&metadata).unwrap().unwrap();
        for layer in Layer::ORDER {
            let doc = layers.get(layer);
            let folded = doc.fold(layer);
            assert_eq!(
                (&doc.labels, &doc.metadata),
                (&folded.labels, &folded.metadata)
            );
        }
        assert_eq!(labels, layers.merged().labels);
    }

    /// A user entry removes a label the producer declared by setting it to
    /// `""`, and a removal made outside `append` is kept the same way.
    #[test]
    fn an_empty_value_removes_a_lower_layers_key() {
        let mut labels = BTreeMap::new();
        let mut metadata = BTreeMap::new();
        put(
            &mut labels,
            &mut metadata,
            Layer::Producer,
            entry("p", "producer", &[("zone", "a"), ("rack", "r1")]),
        );
        put(
            &mut labels,
            &mut metadata,
            Layer::User,
            entry("u", "viewer", &[("zone", "")]),
        );
        assert!(!labels.contains_key("zone"));

        labels.remove("rack");
        put(
            &mut labels,
            &mut metadata,
            Layer::User,
            entry("v", "viewer", &[("tier", "s")]),
        );
        assert!(!labels.contains_key("rack"), "{labels:?}");
        assert_eq!(labels.get("tier").map(String::as_str), Some("s"));
    }

    /// The write that derives a legacy source's layers records no `external`
    /// entry, even where the derivation's fold differs from the source's
    /// top-level metadata (evidence it never held, a `version` that restart
    /// stamps order differently).
    #[test]
    fn deriving_a_legacy_source_records_no_external_entry() {
        let evidence = LegacyEvidence {
            producer_name: Some("rezolus".to_string()),
        };
        let mut labels = map(&[("source", "rezolus")]);
        let mut metadata = map(&[
            ("version", "2"),
            (
                "producer_epochs",
                r#"[{"epoch":"A","from_ts":100,"version":"1"},{"epoch":"B","from_ts":50,"version":"2"}]"#,
            ),
        ]);
        append(
            &mut labels,
            &mut metadata,
            Some("u"),
            &evidence,
            Layer::User,
            entry("x", "viewer", &[("k", "v")]),
        )
        .unwrap();
        let layers = Layers::read(&metadata).unwrap().unwrap();
        assert!(
            layers.user.entries.iter().all(|e| e.by != "external"),
            "{:?}",
            layers.user.entries
        );
        // The source's visible version is its own, not a listed epoch's.
        assert_eq!(metadata["version"], "2");
    }

    /// An entry with no `from_ts` folds before one that has one, whatever
    /// order they were written in.
    #[test]
    fn an_absent_from_ts_folds_first() {
        let mut doc = LayerDoc::default();
        let mut stamped = entry("s", "recorder", &[("k", "stamped")]);
        stamped.from_ts = Some(10);
        stamped.seq = 0;
        let mut open = entry("o", "recorder", &[("k", "open")]);
        open.seq = 1;
        doc.entries = vec![stamped, open];
        assert_eq!(doc.fold(Layer::Recorder).labels["k"], "stamped");
    }

    /// Unsetting a reserved key, writing a key dendro maintains, and a label
    /// under the reserved prefix are refused too.
    #[test]
    fn unsets_dendro_keys_and_reserved_labels_are_refused() {
        let mut unset = Entry::new("a", "annotate");
        unset.unset_metadata = vec!["producer_epochs".to_string()];
        let mut dendro_key = Entry::new("b", "annotate");
        dendro_key.metadata = map(&[("events", "{}")]);
        let label = entry("c", "annotate", &[("metriken.x", "1")]);
        let mut unset_label = Entry::new("d", "annotate");
        unset_label.unset_labels = vec!["metriken.y".to_string()];
        for (e, key) in [
            (unset, "producer_epochs"),
            (dendro_key, "events"),
            (label, "metriken.x"),
            (unset_label, "metriken.y"),
        ] {
            let err = append(
                &mut BTreeMap::new(),
                &mut BTreeMap::new(),
                None,
                &NONE,
                Layer::User,
                e,
            )
            .unwrap_err();
            assert_eq!(err.key, key);
        }
    }

    /// A `producer_epochs` entry whose `version` is not a string still
    /// derives its epochs.
    #[test]
    fn a_non_string_version_keeps_the_list() {
        let metadata = map(&[(
            "producer_epochs",
            r#"[{"epoch":"A","from_ts":10,"version":3},{"epoch":"B","from_ts":20}]"#,
        )]);
        let layers = Layers::derive_legacy(None, &BTreeMap::new(), &metadata, &NONE);
        assert_eq!(layers.recorder.entries.len(), 2);
        assert!(layers.producer.entries.is_empty());
    }

    /// Layered metadata changed and removed outside `append` is kept.
    #[test]
    fn metadata_changed_outside_append_is_kept() {
        let mut labels = BTreeMap::new();
        let mut metadata = BTreeMap::new();
        let mut e = Entry::new("p", "producer");
        e.metadata = map(&[("version", "5"), ("kpi", "q")]);
        put(&mut labels, &mut metadata, Layer::Producer, e);
        metadata.insert("version".to_string(), "6".to_string());
        metadata.remove("kpi");
        put(
            &mut labels,
            &mut metadata,
            Layer::User,
            entry("u", "viewer", &[("k", "v")]),
        );
        assert_eq!(metadata["version"], "6");
        assert!(!metadata.contains_key("kpi"));
    }

    /// A key the user removed with `""` and something then wrote outside
    /// `append` is recorded, so the source and the fold agree.
    #[test]
    fn a_key_written_back_outside_append_is_recorded() {
        let mut labels = BTreeMap::new();
        let mut metadata = BTreeMap::new();
        let mut e = Entry::new("p", "producer");
        e.metadata = map(&[("version", "5")]);
        put(&mut labels, &mut metadata, Layer::Producer, e);
        let mut hide = Entry::new("h", "viewer");
        hide.metadata = map(&[("version", "")]);
        put(&mut labels, &mut metadata, Layer::User, hide);
        assert!(!metadata.contains_key("version"));
        metadata.insert("version".to_string(), "6".to_string());
        put(
            &mut labels,
            &mut metadata,
            Layer::User,
            entry("u", "viewer", &[("k", "v")]),
        );
        assert_eq!(metadata["version"], "6");
        assert_eq!(
            Layers::read(&metadata).unwrap().unwrap().merged().metadata["version"],
            "6"
        );
    }

    /// An empty value from the producer layer removes the key from that
    /// layer only: the recorder layer's value shows, not the producer's
    /// earlier one.
    #[test]
    fn an_empty_producer_value_removes_only_its_layers_key() {
        let mut labels = BTreeMap::new();
        let mut metadata = BTreeMap::new();
        let mut r = Entry::new("r", "recorder");
        r.metadata = map(&[("version", "4.1")]);
        put(&mut labels, &mut metadata, Layer::Recorder, r);
        let mut p = Entry::new("p", "producer");
        p.metadata = map(&[("version", "5")]);
        p.from_ts = Some(10);
        put(&mut labels, &mut metadata, Layer::Producer, p);
        assert_eq!(metadata["version"], "5");
        let mut e = Entry::new("e", "producer");
        e.metadata = map(&[("version", "")]);
        e.from_ts = Some(20);
        put(&mut labels, &mut metadata, Layer::Producer, e);
        assert_eq!(metadata["version"], "4.1");
    }

    /// A user `""` removes a metadata key that only the top level held (one
    /// written outside `append`), and the next write does not restore it.
    #[test]
    fn a_user_removal_of_an_unlayered_key_holds() {
        let mut labels = BTreeMap::new();
        let mut metadata = BTreeMap::new();
        put(
            &mut labels,
            &mut metadata,
            Layer::User,
            entry("a", "viewer", &[("k", "v")]),
        );
        metadata.insert("systeminfo".to_string(), "{}".to_string());
        let mut hide = Entry::new("h", "viewer");
        hide.metadata = map(&[("systeminfo", "")]);
        put(&mut labels, &mut metadata, Layer::User, hide);
        assert!(!metadata.contains_key("systeminfo"));
        put(
            &mut labels,
            &mut metadata,
            Layer::Recorder,
            entry("r", "recorder", &[("x", "1")]),
        );
        assert!(!metadata.contains_key("systeminfo"));
        let layers = Layers::read(&metadata).unwrap().unwrap();
        assert!(
            layers.user.entries.iter().all(|e| e.by != "external"),
            "{:?}",
            layers.user.entries
        );
    }

    /// The evidence's `producer_name` folds over a legacy one, and the
    /// at-open entry is the same with or without evidence, so readers with
    /// different evidence differ only in the evidence's entry.
    #[test]
    fn evidence_overrides_a_legacy_producer_name() {
        let evidence = LegacyEvidence {
            producer_name: Some("new".to_string()),
        };
        let metadata = map(&[("producer_name", "old"), ("k", "v")]);
        // Several uuids, since entries with one `seq` and no `from_ts` fold in
        // the order of their ids' hashes.
        for uuid in ["a", "b", "c", "d", "e", "f", "g", "h"] {
            let with = Layers::derive_legacy(Some(uuid), &BTreeMap::new(), &metadata, &evidence);
            let without = Layers::derive_legacy(Some(uuid), &BTreeMap::new(), &metadata, &NONE);
            assert_eq!(with.merged().metadata["producer_name"], "new", "{uuid}");
            assert_eq!(without.merged().metadata["producer_name"], "old");
            let opened = |l: &Layers| l.recorder.entries[0].clone();
            assert_eq!(opened(&with), opened(&without));
            assert_eq!(
                with.recorder.entries.len(),
                without.recorder.entries.len() + 1
            );
        }
    }

    /// Epochs a legacy list stamps with one `from_ts` keep their list order
    /// through the derivation, as `epoch_spans` reads the flat keys.
    #[test]
    fn epochs_at_one_stamp_keep_their_list_order() {
        for (a, b) in [("A", "B"), ("B", "A"), ("x", "y"), ("y", "x")] {
            let mut labels = BTreeMap::new();
            let mut metadata = map(&[(
                PRODUCER_EPOCHS,
                &format!(r#"[{{"epoch":"{a}","from_ts":5}},{{"epoch":"{b}","from_ts":5}}]"#),
            )]);
            let flat = crate::process::epoch_spans(&metadata, Some((0, 10)));
            put(
                &mut labels,
                &mut metadata,
                Layer::User,
                entry("e", "record", &[("k", "v")]),
            );
            assert!(metadata.contains_key(RECORDER_KEY));
            assert_eq!(crate::process::epoch_spans(&metadata, Some((0, 10))), flat);
        }
    }

    /// A resent handshake stamped before the entry that wins the fold is
    /// appended once, not at every reconnect.
    #[test]
    fn a_resent_handshake_stamped_early_appends_once() {
        let (mut labels, mut metadata) = (BTreeMap::new(), BTreeMap::new());
        let stamped = |id: &str, x: &str, from_ts: u64| Entry {
            from_ts: Some(from_ts),
            ..entry(id, "producer", &[("x", x)])
        };
        assert!(put(
            &mut labels,
            &mut metadata,
            Layer::Producer,
            stamped("a", "1", 10)
        ));
        assert!(put(
            &mut labels,
            &mut metadata,
            Layer::Producer,
            stamped("b", "2", 5)
        ));
        for id in ["c", "d"] {
            assert!(!put(
                &mut labels,
                &mut metadata,
                Layer::Producer,
                stamped(id, "2", 5)
            ));
        }
        let layers = Layers::read(&metadata).unwrap().unwrap();
        assert_eq!(layers.producer.entries.len(), 2);
        assert_eq!(labels["x"], "1");
    }

    /// A label under the reserved prefix written outside `append` is not
    /// taken into the user layer, and the next write drops it.
    #[test]
    fn an_external_reserved_label_is_dropped() {
        let (mut labels, mut metadata) = (BTreeMap::new(), BTreeMap::new());
        put(
            &mut labels,
            &mut metadata,
            Layer::User,
            entry("a", "record", &[("k", "v")]),
        );
        labels.insert("metriken.x".to_string(), "y".to_string());
        put(
            &mut labels,
            &mut metadata,
            Layer::User,
            entry("b", "record", &[("j", "w")]),
        );
        assert_eq!(labels, map(&[("j", "w"), ("k", "v")]));
        let layers = Layers::read(&metadata).unwrap().unwrap();
        assert!(layers.user.entries.iter().all(|e| e.by != "external"));
    }

    /// A layer key that does not parse fails `append` and changes nothing.
    #[test]
    fn a_damaged_layer_fails_append_and_changes_nothing() {
        let mut labels = map(&[("k", "v")]);
        let mut metadata = map(&[(PRODUCER_KEY, "garbage")]);
        let before = (labels.clone(), metadata.clone());
        let err = append(
            &mut labels,
            &mut metadata,
            Some("u"),
            &NONE,
            Layer::User,
            entry("a", "record", &[("j", "w")]),
        )
        .unwrap_err();
        assert_eq!(err.key, PRODUCER_KEY);
        assert_eq!((labels, metadata), before);
    }

    /// A resend of the newest producer entry that would win the fold, once
    /// appended, is appended: the source takes its value.
    #[test]
    fn a_resend_that_would_win_the_fold_is_appended() {
        let (mut labels, mut metadata) = (BTreeMap::new(), BTreeMap::new());
        let stamped = |id: &str, x: &str, from_ts: u64| Entry {
            from_ts: Some(from_ts),
            ..entry(id, "producer", &[("x", x)])
        };
        assert!(put(
            &mut labels,
            &mut metadata,
            Layer::Producer,
            stamped("a", "1", 100)
        ));
        assert!(put(
            &mut labels,
            &mut metadata,
            Layer::Producer,
            stamped("b", "2", 50)
        ));
        assert_eq!(labels["x"], "1");
        assert!(put(
            &mut labels,
            &mut metadata,
            Layer::Producer,
            stamped("c", "2", 200)
        ));
        assert_eq!(labels["x"], "2");
    }

    /// A legacy source's labels under the reserved prefix are left out of
    /// the derived user entry, and the first write drops them.
    #[test]
    fn a_legacy_reserved_label_is_not_derived() {
        let mut labels = map(&[("metriken.x", "y"), ("k", "v")]);
        let mut metadata = BTreeMap::new();
        put(
            &mut labels,
            &mut metadata,
            Layer::User,
            entry("a", "record", &[("j", "w")]),
        );
        assert_eq!(labels, map(&[("j", "w"), ("k", "v")]));
        let layers = Layers::read(&metadata).unwrap().unwrap();
        assert!(layers
            .user
            .entries
            .iter()
            .all(|e| !e.labels.contains_key("metriken.x")));
    }

    /// `derived_id` tells an absent field from an empty one, and does not
    /// give one id to two field lists whose bytes concatenate alike.
    #[test]
    fn derived_ids_separate_fields() {
        let id = |uuid, epoch| derived_id(uuid, Layer::Recorder, epoch, None);
        assert_ne!(id(None, None), id(Some(""), None));
        assert_ne!(id(None, None), id(None, Some("")));
        // Without the presence bytes these two inputs are the same bytes.
        assert_ne!(id(None, Some("recorder")), id(Some("recorder"), None));
        assert_ne!(id(Some("ab"), Some("c")), id(Some("a"), Some("bc")));
        // Without the lengths these two inputs are the same bytes, the layer
        // name `recorder` sitting between the uuid and the epoch.
        assert_ne!(
            id(Some("a\u{1}recorder"), Some("b")),
            id(Some("a"), Some("recorder\u{1}b"))
        );
        assert_ne!(
            derived_id(None, Layer::Recorder, None, None),
            derived_id(None, Layer::Recorder, None, Some(0))
        );
    }

    /// A resend of the producer entry appended last that changes its own
    /// epoch's fold is appended, though the whole layer's fold is unchanged.
    #[test]
    fn a_resend_that_changes_its_epochs_fold_is_appended() {
        let (mut labels, mut metadata) = (BTreeMap::new(), BTreeMap::new());
        let stamped = |id: &str, x: &str, epoch: &str, from_ts: u64| Entry {
            from_ts: Some(from_ts),
            epoch: Some(epoch.to_string()),
            ..entry(id, "producer", &[("x", x)])
        };
        for e in [
            stamped("a", "1", "E", 100),
            stamped("c", "3", "F", 300),
            stamped("b", "2", "E", 50),
        ] {
            assert!(put(&mut labels, &mut metadata, Layer::Producer, e));
        }
        assert!(put(
            &mut labels,
            &mut metadata,
            Layer::Producer,
            stamped("d", "2", "E", 200)
        ));
        assert_eq!(labels["x"], "3");
    }

    /// A `descriptions` value rewritten outside `append` with a key removed,
    /// or its keys reordered, is not recorded as a change.
    #[test]
    fn a_descriptions_subset_written_outside_append_is_no_change() {
        let (mut labels, mut metadata) = (BTreeMap::new(), BTreeMap::new());
        let mut producer = Entry::new("p", "producer");
        producer.metadata = map(&[("descriptions", r#"{"a":"A","b":"B"}"#)]);
        put(&mut labels, &mut metadata, Layer::Producer, producer);
        for (i, rewrite) in [r#"{"a":"A"}"#, r#"{"b":"B","a":"A"}"#]
            .into_iter()
            .enumerate()
        {
            metadata.insert("descriptions".to_string(), rewrite.to_string());
            let id = format!("u{i}");
            put(
                &mut labels,
                &mut metadata,
                Layer::User,
                entry(&id, "record", &[("k", "v")]),
            );
        }
        let layers = Layers::read(&metadata).unwrap().unwrap();
        assert!(layers.user.entries.iter().all(|e| e.by != "external"));
        // A changed value is still a change.
        metadata.insert("descriptions".to_string(), r#"{"a":"Z"}"#.to_string());
        put(
            &mut labels,
            &mut metadata,
            Layer::User,
            entry("u9", "record", &[("k", "w")]),
        );
        let layers = Layers::read(&metadata).unwrap().unwrap();
        assert_eq!(
            layers
                .user
                .entries
                .iter()
                .filter(|e| e.by == "external")
                .count(),
            1
        );
    }

    /// A resend of the producer entry appended last that changes the layer's
    /// fold is appended, though its epoch's fold is unchanged.
    #[test]
    fn a_resend_that_changes_the_layers_fold_is_appended() {
        let (mut labels, mut metadata) = (BTreeMap::new(), BTreeMap::new());
        let stamped = |id: &str, x: &str, epoch: &str, from_ts: u64| Entry {
            from_ts: Some(from_ts),
            epoch: Some(epoch.to_string()),
            ..entry(id, "producer", &[("x", x)])
        };
        for e in [
            stamped("a", "2", "E", 100),
            stamped("c", "3", "F", 300),
            stamped("b", "2", "E", 50),
        ] {
            assert!(put(&mut labels, &mut metadata, Layer::Producer, e));
        }
        assert_eq!(labels["x"], "3");
        assert!(put(
            &mut labels,
            &mut metadata,
            Layer::Producer,
            stamped("d", "2", "E", 500)
        ));
        assert_eq!(labels["x"], "2");
    }

    /// A JSON object under a key other than `descriptions`, rewritten outside
    /// `append` with a key removed, is a change, and the rewrite is kept.
    #[test]
    fn a_subset_of_another_json_key_written_outside_append_is_a_change() {
        let (mut labels, mut metadata) = (BTreeMap::new(), BTreeMap::new());
        let mut producer = Entry::new("p", "producer");
        producer.metadata = map(&[("config", r#"{"a":"A","b":"B"}"#)]);
        put(&mut labels, &mut metadata, Layer::Producer, producer);
        metadata.insert("config".to_string(), r#"{"a":"A"}"#.to_string());
        put(
            &mut labels,
            &mut metadata,
            Layer::User,
            entry("u", "record", &[("k", "v")]),
        );
        let layers = Layers::read(&metadata).unwrap().unwrap();
        assert_eq!(
            layers
                .user
                .entries
                .iter()
                .filter(|e| e.by == "external")
                .count(),
            1
        );
        assert_eq!(metadata["config"], r#"{"a":"A"}"#);
    }

    /// The dedupe's fold check places the entry by the `seq` it is given, so
    /// a resend tied on `from_ts` with the fold winner, which folds after it,
    /// is appended.
    #[test]
    fn a_resend_tied_with_the_winner_is_placed_by_its_seq() {
        let (mut labels, mut metadata) = (BTreeMap::new(), BTreeMap::new());
        let stamped = |id: &str, x: &str, from_ts: u64| Entry {
            from_ts: Some(from_ts),
            ..entry(id, "producer", &[("x", x)])
        };
        assert!(put(
            &mut labels,
            &mut metadata,
            Layer::Producer,
            stamped("w", "1", 200)
        ));
        assert!(put(
            &mut labels,
            &mut metadata,
            Layer::Producer,
            stamped("n", "2", 100)
        ));
        assert_eq!(labels["x"], "1");
        assert!(put(
            &mut labels,
            &mut metadata,
            Layer::Producer,
            stamped("r", "2", 200)
        ));
        assert_eq!(labels["x"], "2");
    }

    /// A layer whose highest `seq` is `u64::MAX` refuses a further entry
    /// rather than wrapping.
    #[test]
    fn a_layer_at_the_last_seq_refuses_an_entry() {
        let (mut labels, mut metadata) = (BTreeMap::new(), BTreeMap::new());
        put(
            &mut labels,
            &mut metadata,
            Layer::User,
            entry("a", "record", &[("k", "v")]),
        );
        let mut layers = Layers::read(&metadata).unwrap().unwrap();
        layers.user.entries[0].seq = u64::MAX;
        metadata.insert(
            USER_KEY.to_string(),
            serde_json::to_string(&layers.user).unwrap(),
        );
        let err = append(
            &mut labels,
            &mut metadata,
            Some("u"),
            &NONE,
            Layer::User,
            entry("b", "record", &[("j", "w")]),
        )
        .unwrap_err();
        assert_eq!(err.key, USER_KEY);
    }

    /// A producer entry with the fold winner's labels and metadata in
    /// another epoch, or with different unset lists, is appended: neither
    /// repeats it.
    #[test]
    fn the_dedupe_compares_epoch_and_unset_lists() {
        let (mut labels, mut metadata) = (BTreeMap::new(), BTreeMap::new());
        let with_epoch = |id: &str, epoch: &str| Entry {
            epoch: Some(epoch.to_string()),
            ..entry(id, "producer", &[("x", "1")])
        };
        assert!(put(
            &mut labels,
            &mut metadata,
            Layer::Producer,
            with_epoch("a", "E")
        ));
        assert!(put(
            &mut labels,
            &mut metadata,
            Layer::Producer,
            with_epoch("b", "F")
        ));
        let mut unsets = with_epoch("c", "F");
        unsets.unset_metadata = vec!["k".to_string()];
        assert!(put(&mut labels, &mut metadata, Layer::Producer, unsets));
        let layers = Layers::read(&metadata).unwrap().unwrap();
        assert_eq!(layers.producer.entries.len(), 3);
    }

    /// A user or recorder entry repeating its layer's fold winner is
    /// appended: only the producer layer is deduplicated.
    #[test]
    fn only_the_producer_layer_is_deduplicated() {
        for layer in [Layer::User, Layer::Recorder] {
            let (mut labels, mut metadata) = (BTreeMap::new(), BTreeMap::new());
            for id in ["a", "b"] {
                assert!(put(
                    &mut labels,
                    &mut metadata,
                    layer,
                    entry(id, "record", &[("k", "v")])
                ));
            }
            let layers = Layers::read(&metadata).unwrap().unwrap();
            assert_eq!(layers.get(layer).entries.len(), 2, "{layer:?}");
        }
    }

    /// The appended entry takes a `seq` after the `external` entry recorded
    /// before it, so it folds after it whatever the ids.
    #[test]
    fn an_entry_folds_after_the_external_change_before_it() {
        let (mut labels, mut metadata) = (BTreeMap::new(), BTreeMap::new());
        put(
            &mut labels,
            &mut metadata,
            Layer::User,
            entry("z", "record", &[("k", "v")]),
        );
        labels.insert("k".to_string(), "outside".to_string());
        put(
            &mut labels,
            &mut metadata,
            Layer::User,
            entry("a", "record", &[("k", "mine")]),
        );
        assert_eq!(labels["k"], "mine");
    }

    /// A legacy source whose metadata declares a producer identity under
    /// `metriken.producer.*` derives a producer entry from it; the declared
    /// keys stay in its metadata.
    #[test]
    fn a_declared_producer_identity_is_derived() {
        let mut labels = map(&[("source", "svc")]);
        let mut metadata = map(&[
            ("metriken.producer.name", "cachecannon"),
            ("metriken.producer.version", "0.4.0"),
            ("metriken.producer.host", "lg-1"),
            ("metriken.producer.build", "c0ffee"),
            ("version", "0.3.0"),
        ]);
        put(
            &mut labels,
            &mut metadata,
            Layer::User,
            entry("a", "record", &[("k", "v")]),
        );
        assert_eq!(labels["host"], "lg-1");
        assert_eq!(metadata[PRODUCER_NAME], "cachecannon");
        assert_eq!(metadata[VERSION], "0.4.0");
        assert_eq!(metadata[BUILD], "c0ffee");
        assert_eq!(metadata["metriken.producer.name"], "cachecannon");
        let layers = Layers::read(&metadata).unwrap().unwrap();
        assert_eq!(layers.producer.entries.len(), 1);
        assert_eq!(layers.producer.entries[0].by, "upgrade");
    }

    /// `identity_keys` replaces what the maps held under its keys.
    #[test]
    fn identity_keys_replace_the_maps_values() {
        let identity = ProducerIdentity::new("new")
            .with_version("2")
            .with_host("h");
        let mut labels = map(&[("host", "old")]);
        let mut metadata = map(&[(PRODUCER_NAME, "old"), (VERSION, "1"), ("k", "v")]);
        identity_keys(&identity, &mut labels, &mut metadata);
        assert_eq!(labels, map(&[("host", "h")]));
        assert_eq!(
            metadata,
            map(&[(PRODUCER_NAME, "new"), (VERSION, "2"), ("k", "v")])
        );
    }

    /// An outside change to `descriptions` is recorded as the names it adds
    /// or changes, so a later producer value of another name still shows.
    #[test]
    fn an_external_descriptions_change_keeps_only_what_changed() {
        let (mut labels, mut metadata) = (BTreeMap::new(), BTreeMap::new());
        let described = |id: &str, json: &str| {
            let mut e = Entry::new(id, "producer");
            e.metadata = map(&[("descriptions", json)]);
            e
        };
        put(
            &mut labels,
            &mut metadata,
            Layer::Producer,
            described("a", r#"{"a":"A","b":"B"}"#),
        );
        metadata.insert(
            "descriptions".to_string(),
            r#"{"a":"A","b":"B2","z":"Z"}"#.to_string(),
        );
        put(
            &mut labels,
            &mut metadata,
            Layer::User,
            entry("u", "record", &[("k", "v")]),
        );
        let layers = Layers::read(&metadata).unwrap().unwrap();
        let external = layers
            .user
            .entries
            .iter()
            .find(|e| e.by == "external")
            .unwrap();
        assert_eq!(external.metadata["descriptions"], r#"{"b":"B2","z":"Z"}"#);
        put(
            &mut labels,
            &mut metadata,
            Layer::Producer,
            described("b", r#"{"a":"NEW"}"#),
        );
        let merged: BTreeMap<String, String> =
            serde_json::from_str(&metadata["descriptions"]).unwrap();
        assert_eq!(merged, map(&[("a", "NEW"), ("b", "B2"), ("z", "Z")]));
    }
}
