//! The entries the recorder writes into a source's identity layers
//! (metriken-storage's [`layers`](metriken_storage::layers)).
//!
//! A source's labels and metadata are the layers merged: what the recorder
//! observed (the recorder layer), what the producer declared (the producer
//! layer), and what the caller gave (the user layer), the last winning on a
//! key. The recorder writes no `source` of its own.

use std::collections::hash_map::RandomState;
use std::collections::BTreeMap;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

use metriken_model::identity::ProducerIdentity;
use metriken_storage::layers::{self, Entry, Layer, LayerError, LegacyEvidence, RESERVED_PREFIX};

use crate::keys;

/// The recorder layer's metadata key naming how the source was recorded:
/// `stream`, `prometheus` or `local`.
pub(crate) const TRANSPORT: &str = "transport";

/// The recorder layer's label naming the endpoint a source was recorded
/// from: its URL as parsed ([`instance`]), without a user name or password.
pub(crate) const INSTANCE: &str = "instance";

/// Keys the recorder sets in the recorder layer itself, which a probe hook's
/// reply does not replace.
const RECORDER_LABELS: [&str; 1] = [INSTANCE];
const RECORDER_METADATA: [&str; 2] = [keys::SAMPLING_INTERVAL_MS, TRANSPORT];

/// dendro's timeline-events key. A caller writes it whole (`rezolus record`
/// writes its run markers there); the recorder stores it as given, outside
/// the layers, since an entry may not carry it.
pub(crate) const EVENTS: &str = dendro::keys::EVENTS;

/// `map` without [`EVENTS`], and its `EVENTS` value.
pub(crate) fn split_events(
    map: &BTreeMap<String, String>,
) -> (BTreeMap<String, String>, Option<String>) {
    let mut rest = map.clone();
    let events = rest.remove(EVENTS);
    (rest, events)
}

/// A source's labels and metadata as last written.
#[derive(Clone, Debug, Default)]
pub(crate) struct Maps {
    pub labels: BTreeMap<String, String>,
    pub metadata: BTreeMap<String, String>,
}

impl Maps {
    /// Append `entry` to `layer`, rewriting both maps. Whether anything was
    /// appended (a producer entry repeating the one on record is not).
    pub fn append(&mut self, layer: Layer, entry: Entry) -> Result<bool, LayerError> {
        layers::append(
            &mut self.labels,
            &mut self.metadata,
            None,
            &LegacyEvidence::default(),
            layer,
            entry,
        )
    }
}

/// The `instance` label for an endpoint at `url`: the URL as parsed, without
/// the user name and password the recorder may have sent, since a recording
/// is read by others. The query string is kept: it can name what is
/// scraped (a blackbox exporter's `module`).
#[cfg(feature = "http")]
pub(crate) fn instance(url: &url::Url) -> String {
    let mut url = url.clone();
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.to_string()
}

/// Of the `descriptions` JSON object `offered`, the entries `held` (the
/// recording's merged `descriptions`) does not already have with the same
/// value, as a JSON object. `offered` as it is when either is not an object.
#[cfg(feature = "http")]
pub(crate) fn new_descriptions(offered: &str, held: Option<&str>) -> String {
    use serde_json::{Map, Value};
    let parse = |s: &str| match serde_json::from_str::<Value>(s) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    };
    let (Some(offered_map), Some(held)) = (parse(offered), held.and_then(parse)) else {
        return offered.to_string();
    };
    let added: Map<String, Value> = offered_map
        .into_iter()
        .filter(|(k, v)| held.get(k) != Some(v))
        .collect();
    Value::Object(added).to_string()
}

/// A new entry by `by`, with a random id and the wall clock now as its `ts`.
pub(crate) fn entry(by: &str) -> Entry {
    let mut entry = Entry::new(random_id(), by);
    entry.ts = Some(crate::recorder::wall_now_ns());
    entry
}

/// 128 random bits as hex. `RandomState` is seeded randomly per process and
/// per instance; the counter and the clock make two calls in one instant
/// differ.
fn random_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let half = || {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        h.write_u64(crate::recorder::wall_now_ns());
        h.finish()
    };
    format!("{:016x}{:016x}", half(), half())
}

/// The recorder's entry for a source as it is added: `instance` (an
/// endpoint's URL), `transport`, `sampling_interval_ms`, and the probe
/// hook's labels and metadata under those, without a hook `source` label.
pub(crate) fn opened(
    instance: Option<&str>,
    transport: &str,
    interval_ms: String,
    hook_labels: &BTreeMap<String, String>,
    hook_metadata: &BTreeMap<String, String>,
) -> Entry {
    let mut e = entry("recorder");
    e.labels = hook(hook_labels, true);
    e.metadata = hook(hook_metadata, false);
    if let Some(instance) = instance {
        e.labels.insert(INSTANCE.to_string(), instance.to_string());
    }
    e.metadata
        .insert(TRANSPORT.to_string(), transport.to_string());
    e.metadata
        .insert(keys::SAMPLING_INTERVAL_MS.to_string(), interval_ms);
    e
}

/// A recorder entry for producer epoch `epoch`, from `from_ts` (`None` for
/// the epoch the source opened with), with the probe hook's metadata for
/// that process.
pub(crate) fn epoch(
    epoch: &str,
    from_ts: Option<u64>,
    hook_metadata: &BTreeMap<String, String>,
) -> Entry {
    let mut e = entry("recorder");
    e.epoch = Some(epoch.to_string());
    e.from_ts = from_ts;
    e.metadata = hook(hook_metadata, false);
    e
}

/// A recorder entry for what the probe hook replied after the source was
/// added. It carries no epoch: only an epoch's own entry delimits a span.
#[cfg(feature = "http")]
pub(crate) fn late_hook(hook_metadata: &BTreeMap<String, String>) -> Entry {
    let mut e = entry("recorder");
    e.metadata = hook(hook_metadata, false);
    e
}

/// The probe hook's keys the recorder layer takes: not `source` (the
/// recorder writes none), not the recorder's own keys (the `instance` label;
/// `transport` and `sampling_interval_ms`), and no metadata key
/// `layers::forbidden_metadata` names (`metriken.` keys, the flat epoch
/// keys, dendro's `events`, `encoder` and `writer_sessions`).
fn hook(map: &BTreeMap<String, String>, labels: bool) -> BTreeMap<String, String> {
    map.iter()
        .filter(|(k, _)| {
            let k = k.as_str();
            k != keys::SOURCE
                && if labels {
                    !refused_label(k) && !RECORDER_LABELS.contains(&k)
                } else {
                    !layers::forbidden_metadata(k) && !RECORDER_METADATA.contains(&k)
                }
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// A producer entry: the labels and metadata a producer declared (its stream
/// handshake, or a local source's identity), for epoch `epoch` from
/// `from_ts`. A producer identity, declared under `metriken.producer.*` or
/// given as `identity`, becomes metadata `producer_name`, `version` and
/// `build` and the label `host`; the identity's version and name replace
/// the declared `version` and `producer_name`. Left out: the recorder's own
/// keys (the `instance` label; `transport` and `sampling_interval_ms`),
/// which are its observation and not the producer's to replace, and the
/// metadata keys `layers::forbidden_metadata` names.
pub(crate) fn producer(
    labels: &BTreeMap<String, String>,
    metadata: &BTreeMap<String, String>,
    identity: Option<&ProducerIdentity>,
    epoch: Option<&str>,
    from_ts: Option<u64>,
) -> Entry {
    let declared = ProducerIdentity::from_metadata(metadata);
    let identity = identity.or(declared.as_ref());
    let mut e = entry("producer");
    e.epoch = epoch.map(str::to_string);
    e.from_ts = from_ts;
    e.labels = labels
        .iter()
        .filter(|(k, _)| !refused_label(k) && !RECORDER_LABELS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    e.metadata = metadata
        .iter()
        .filter(|(k, _)| !layers::forbidden_metadata(k) && !RECORDER_METADATA.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if let Some(id) = identity {
        layers::identity_keys(id, &mut e.labels, &mut e.metadata);
    }
    e
}

/// A user entry by `record`: labels and metadata the caller gave.
pub(crate) fn user(
    labels: &BTreeMap<String, String>,
    metadata: &BTreeMap<String, String>,
) -> Entry {
    let mut e = entry("record");
    e.labels = labels.clone();
    e.metadata = metadata.clone();
    e
}

/// Whether a label is one the recorder refuses from a caller and leaves out
/// of a producer's or hook's entry: one under `metriken.`. Metadata keys
/// are filtered by `layers::forbidden_metadata`.
pub(crate) fn refused_label(key: &str) -> bool {
    key.starts_with(RESERVED_PREFIX)
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

    #[test]
    fn a_handshake_identity_becomes_producer_keys() {
        let metadata = map(&[
            ("metriken.producer.name", "rezolus"),
            ("metriken.producer.version", "5.21.0"),
            ("metriken.producer.host", "db-1"),
            ("metriken.frame_interval", "1s"),
            ("producer_epoch", "e1"),
            ("dashboard", "{}"),
        ]);
        let e = producer(
            &map(&[("source", "rezolus")]),
            &metadata,
            None,
            Some("e1"),
            None,
        );
        assert_eq!(e.labels, map(&[("host", "db-1"), ("source", "rezolus")]));
        assert_eq!(
            e.metadata,
            map(&[
                ("dashboard", "{}"),
                ("producer_name", "rezolus"),
                ("version", "5.21.0"),
            ])
        );
        assert_eq!(e.epoch.as_deref(), Some("e1"));
    }

    #[test]
    fn a_given_identity_outranks_the_declared_one() {
        let metadata = map(&[("metriken.producer.name", "a"), ("version", "1")]);
        let given = ProducerIdentity::new("b")
            .with_version("2")
            .with_build("c0ffee");
        let e = producer(&BTreeMap::new(), &metadata, Some(&given), None, None);
        assert_eq!(
            e.metadata,
            map(&[
                ("build", "c0ffee"),
                ("producer_name", "b"),
                ("version", "2")
            ])
        );
    }

    #[test]
    fn the_hook_does_not_name_the_source_or_replace_the_recorders_keys() {
        let e = opened(
            Some("http://h:4241"),
            "stream",
            "1000".to_string(),
            &map(&[
                ("source", "rezolus"),
                ("zone", "a"),
                ("metriken.x", "y"),
                ("instance", "theirs"),
                ("transport", "udp"),
            ]),
            &map(&[
                ("sampling_interval_ms", "5"),
                ("transport", "x"),
                ("source", "rezolus"),
                ("instance", "kept"),
                ("agent_version", "5.2"),
                ("metriken.x", "y"),
                ("events", "{}"),
            ]),
        );
        assert_eq!(
            e.labels,
            map(&[
                ("instance", "http://h:4241"),
                ("transport", "udp"),
                ("zone", "a")
            ])
        );
        assert_eq!(
            e.metadata,
            map(&[
                ("agent_version", "5.2"),
                ("instance", "kept"),
                ("sampling_interval_ms", "1000"),
                ("transport", "stream")
            ])
        );
        // A restart's recorder entry filters the hook's reply the same way.
        let e = epoch(
            "e2",
            Some(5),
            &map(&[
                ("sampling_interval_ms", "5"),
                ("source", "rezolus"),
                ("version", "2"),
            ]),
        );
        assert_eq!(e.metadata, map(&[("version", "2")]));
    }

    #[test]
    fn a_handshake_does_not_replace_the_recorders_keys() {
        let e = producer(
            &map(&[
                ("instance", "theirs"),
                ("zone", "a"),
                ("transport", "udp"),
                ("metriken.x", "y"),
            ]),
            &map(&[
                ("sampling_interval_ms", "5"),
                ("transport", "x"),
                ("instance", "kept"),
                ("note", "n"),
            ]),
            None,
            None,
            None,
        );
        assert_eq!(e.labels, map(&[("transport", "udp"), ("zone", "a")]));
        assert_eq!(e.metadata, map(&[("instance", "kept"), ("note", "n")]));
    }

    #[cfg(feature = "http")]
    #[test]
    fn instance_leaves_out_credentials() {
        let url = url::Url::parse("http://user:secret@Host:9090/metrics?module=x").unwrap();
        assert_eq!(instance(&url), "http://host:9090/metrics?module=x");
    }

    #[cfg(feature = "http")]
    #[test]
    fn only_new_descriptions_are_written() {
        assert_eq!(
            new_descriptions(
                r#"{"a":"A","b":"B","c":"C2"}"#,
                Some(r#"{"a":"A","c":"C"}"#)
            ),
            r#"{"b":"B","c":"C2"}"#
        );
        assert_eq!(new_descriptions(r#"{"a":"A"}"#, None), r#"{"a":"A"}"#);
    }

    #[test]
    fn ids_differ() {
        assert_ne!(random_id(), random_id());
        assert_eq!(random_id().len(), 32);
    }
}
