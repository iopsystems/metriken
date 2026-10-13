//! A recording's metadata after its producer restarts mid-recording.
//!
//! When a streamed producer restarts, every cumulative counter starts again
//! from zero and the new process may be a different build. The reconnect's
//! handshake names a new epoch; [`metadata_patch`] is what changes in the
//! recording's metadata: the epoch history, and what the new process reports
//! about itself.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use crate::keys;

/// A restart seen on a source, waiting for its first row and then for the
/// new process's metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Restart {
    /// The new process's producer epoch.
    pub epoch: String,
    /// The timestamp of the first row staged from the new process; `None`
    /// until one is.
    pub from_ts: Option<u64>,
    /// Whether any row had been recorded when the restart was seen. Taken
    /// then rather than worked out from timestamps, since the old and new
    /// processes' clocks need not agree.
    pub rows_before: bool,
}

/// How many times the probe hook is asked for the restarted producer's
/// metadata, one tick apart, before the restart is written without it.
pub const FETCH_ATTEMPTS: u32 = 3;

/// The keys the recorder writes itself, which a restarted producer's
/// metadata never replaces.
pub(crate) const RECORDER_KEYS: [&str; 4] = [
    keys::SOURCE,
    keys::SAMPLING_INTERVAL_MS,
    keys::PRODUCER_EPOCH,
    keys::PRODUCER_EPOCHS,
];

/// The keys to patch into a recording's metadata, `current`, for the
/// restarts in `restarts` (oldest first, each with its `from_ts`).
/// `reported` is what the newest process reports about itself: its
/// handshake's metadata under the probe hook's reply. `first_ts` is the
/// recording's first row, the start of the epoch `current` was opened with.
/// `pinned` names the keys the caller owns (the endpoint's metadata, and
/// every key it patched), which a restart leaves alone.
///
/// - `producer_epoch` becomes the newest epoch.
/// - `producer_epochs` gains one entry per restart. The first restart seeds
///   it with the epoch the recording opened with, from `first_ts`, when a row
///   from that epoch was recorded before the restart. An entry carries the
///   process's `version` when it reported one and `version` is not pinned.
/// - Every other key in `reported`, except the recorder's own, becomes
///   `merge(key, old, new)` where the recording has a value, and the new
///   value where it has none.
pub fn metadata_patch(
    current: &BTreeMap<String, String>,
    first_ts: Option<u64>,
    restarts: &[Restart],
    reported: &BTreeMap<String, String>,
    pinned: &BTreeSet<String>,
    merge: &dyn Fn(&str, &str, &str) -> String,
) -> BTreeMap<String, String> {
    let version_pinned = pinned.contains(keys::VERSION);
    let mut patch = BTreeMap::new();
    let Some(newest) = restarts.last() else {
        return patch;
    };

    let mut epochs: Vec<Value> = current
        .get(keys::PRODUCER_EPOCHS)
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    // The opening epoch has rows only if one was recorded before the first
    // restart was seen.
    let opened_with_rows = first_ts.filter(|_| restarts[0].rows_before);
    if let (true, Some(first), Some(epoch)) = (
        epochs.is_empty(),
        opened_with_rows,
        current.get(keys::PRODUCER_EPOCH),
    ) {
        // A pinned `version` is the caller's label, not what this process
        // reported, so its entry has none.
        let version = (!version_pinned)
            .then(|| current.get(keys::VERSION))
            .flatten();
        epochs.push(entry(epoch, first, version));
    }
    let new_version = (!version_pinned)
        .then(|| reported.get(keys::VERSION))
        .flatten();
    for (i, restart) in restarts.iter().enumerate() {
        let last = epochs
            .last()
            .and_then(|e| e.get("epoch"))
            .and_then(Value::as_str);
        if last == Some(restart.epoch.as_str()) {
            continue;
        }
        // Only the newest process could be asked for its version; an epoch
        // that came and went between two reads has none on record.
        let version = (i + 1 == restarts.len()).then_some(new_version).flatten();
        epochs.push(entry(
            &restart.epoch,
            restart.from_ts.or(first_ts).unwrap_or_default(),
            version,
        ));
    }
    patch.insert(
        keys::PRODUCER_EPOCHS.to_string(),
        Value::Array(epochs).to_string(),
    );
    patch.insert(keys::PRODUCER_EPOCH.to_string(), newest.epoch.clone());

    for (key, value) in reported {
        if RECORDER_KEYS.contains(&key.as_str()) || pinned.contains(key) {
            continue;
        }
        let merged = match current.get(key) {
            Some(old) => merge(key, old, value),
            None => value.clone(),
        };
        patch.insert(key.clone(), merged);
    }
    patch
}

fn entry(epoch: &str, from_ts: u64, version: Option<&String>) -> Value {
    let mut e = json!({ "epoch": epoch, "from_ts": from_ts });
    if let Some(version) = version {
        e["version"] = Value::String(version.clone());
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYSTEMINFO: &str = "systeminfo";
    const DESCRIPTIONS: &str = keys::DESCRIPTIONS;

    fn opened(epoch: &str, version: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            (keys::PRODUCER_EPOCH.to_string(), epoch.to_string()),
            (keys::VERSION.to_string(), version.to_string()),
            (
                DESCRIPTIONS.to_string(),
                r#"{"a":"old a","b":"old b"}"#.to_string(),
            ),
            (SYSTEMINFO.to_string(), r#"{"host":"old"}"#.to_string()),
        ])
    }

    fn restart(epoch: &str, from_ts: u64) -> Restart {
        Restart {
            epoch: epoch.to_string(),
            from_ts: Some(from_ts),
            rows_before: true,
        }
    }

    fn reported(version: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            (keys::VERSION.to_string(), version.to_string()),
            (SYSTEMINFO.to_string(), r#"{"host":"new"}"#.to_string()),
            (
                DESCRIPTIONS.to_string(),
                r#"{"b":"new b","c":"new c"}"#.to_string(),
            ),
        ])
    }

    /// A merge that unions JSON objects for `descriptions`, as rezolus's
    /// hook does, and takes the new value otherwise.
    fn union(key: &str, old: &str, new: &str) -> String {
        if key != DESCRIPTIONS {
            return new.to_string();
        }
        let parse = |s: &str| serde_json::from_str::<serde_json::Map<String, Value>>(s).ok();
        match (parse(old), parse(new)) {
            (Some(mut merged), Some(new)) => {
                merged.extend(new);
                Value::Object(merged).to_string()
            }
            _ => new.to_string(),
        }
    }

    fn patch(
        current: &BTreeMap<String, String>,
        first_ts: Option<u64>,
        restarts: &[Restart],
        reported: &BTreeMap<String, String>,
        pinned: &[&str],
    ) -> BTreeMap<String, String> {
        let pinned = pinned.iter().map(|k| k.to_string()).collect();
        metadata_patch(current, first_ts, restarts, reported, &pinned, &union)
    }

    fn epochs(patch: &BTreeMap<String, String>) -> Value {
        serde_json::from_str(&patch[keys::PRODUCER_EPOCHS]).unwrap()
    }

    #[test]
    fn a_first_restart_seeds_the_history_with_the_opening_epoch() {
        let p = patch(
            &opened("e1", "6.0.0"),
            Some(100),
            &[restart("e2", 500)],
            &reported("6.0.1"),
            &[],
        );
        assert_eq!(
            epochs(&p),
            json!([
                {"epoch": "e1", "from_ts": 100, "version": "6.0.0"},
                {"epoch": "e2", "from_ts": 500, "version": "6.0.1"},
            ])
        );
        assert_eq!(p[keys::PRODUCER_EPOCH], "e2");
        assert_eq!(p[keys::VERSION], "6.0.1");
        assert_eq!(p[SYSTEMINFO], r#"{"host":"new"}"#);
    }

    #[test]
    fn a_key_the_recording_has_goes_through_the_hooks_merge() {
        let p = patch(
            &opened("e1", "6.0.0"),
            Some(100),
            &[restart("e2", 500)],
            &reported("6.0.1"),
            &[],
        );
        let d: Value = serde_json::from_str(&p[DESCRIPTIONS]).unwrap();
        assert_eq!(d, json!({"a": "old a", "b": "new b", "c": "new c"}));
    }

    #[test]
    fn a_later_restart_appends_to_the_history_already_written() {
        let mut current = opened("e1", "6.0.0");
        current.extend(patch(
            &current,
            Some(100),
            &[restart("e2", 500)],
            &reported("6.0.1"),
            &[],
        ));
        let p = patch(
            &current,
            Some(100),
            &[restart("e3", 900)],
            &reported("6.0.2"),
            &[],
        );
        let e = epochs(&p);
        assert_eq!(e.as_array().unwrap().len(), 3);
        assert_eq!(
            e[2],
            json!({"epoch": "e3", "from_ts": 900, "version": "6.0.2"})
        );
    }

    #[test]
    fn two_restarts_before_one_read_leave_the_middle_epoch_without_a_version() {
        let p = patch(
            &opened("e1", "6.0.0"),
            Some(100),
            &[restart("e2", 500), restart("e3", 700)],
            &reported("6.0.2"),
            &[],
        );
        assert_eq!(
            epochs(&p),
            json!([
                {"epoch": "e1", "from_ts": 100, "version": "6.0.0"},
                {"epoch": "e2", "from_ts": 500},
                {"epoch": "e3", "from_ts": 700, "version": "6.0.2"},
            ])
        );
    }

    /// A pinned key survives a restart; an unpinned one takes the new
    /// process's value. A pinned `version` is not what the first process
    /// reported, so its epoch entry has none.
    #[test]
    fn pinned_keys_are_kept() {
        let p = patch(
            &opened("e1", "pinned"),
            Some(100),
            &[restart("e2", 500)],
            &reported("6.0.1"),
            &[SYSTEMINFO, DESCRIPTIONS, keys::VERSION],
        );
        assert!(!p.contains_key(SYSTEMINFO), "{p:?}");
        assert!(!p.contains_key(DESCRIPTIONS), "{p:?}");
        assert!(!p.contains_key(keys::VERSION), "{p:?}");
        assert_eq!(
            epochs(&p),
            json!([{"epoch": "e1", "from_ts": 100}, {"epoch": "e2", "from_ts": 500}])
        );
    }

    /// The recorder's own keys are never taken from a producer's metadata.
    #[test]
    fn the_recorders_keys_are_not_reported_over() {
        let mut reported = reported("6.0.1");
        reported.insert(keys::SOURCE.into(), "other".into());
        reported.insert(keys::SAMPLING_INTERVAL_MS.into(), "5".into());
        reported.insert(keys::PRODUCER_EPOCH.into(), "forged".into());
        let p = patch(
            &opened("e1", "6.0.0"),
            Some(100),
            &[restart("e2", 500)],
            &reported,
            &[],
        );
        assert!(!p.contains_key(keys::SOURCE));
        assert!(!p.contains_key(keys::SAMPLING_INTERVAL_MS));
        assert_eq!(p[keys::PRODUCER_EPOCH], "e2");
    }

    #[test]
    fn an_opening_epoch_with_no_rows_before_the_restart_gets_no_entry() {
        let no_rows = Restart {
            rows_before: false,
            ..restart("e2", 500)
        };
        for first_ts in [Some(500), None] {
            let p = patch(
                &opened("e1", "6.0.0"),
                first_ts,
                std::slice::from_ref(&no_rows),
                &reported("6.0.1"),
                &[],
            );
            assert_eq!(
                epochs(&p),
                json!([{"epoch": "e2", "from_ts": 500, "version": "6.0.1"}]),
                "first_ts {first_ts:?}"
            );
        }
    }

    #[test]
    fn a_process_that_reported_nothing_changes_only_the_epochs() {
        let p = patch(
            &opened("e1", "6.0.0"),
            Some(100),
            &[restart("e2", 500)],
            &BTreeMap::new(),
            &[],
        );
        let keys_: Vec<&str> = p.keys().map(String::as_str).collect();
        assert_eq!(keys_, [keys::PRODUCER_EPOCH, keys::PRODUCER_EPOCHS]);
    }

    #[test]
    fn the_keys_match_dendro() {
        assert_eq!(keys::PRODUCER_EPOCH, dendro::keys::PRODUCER_EPOCH);
        assert_eq!(keys::PRODUCER_EPOCHS, dendro::keys::PRODUCER_EPOCHS);
    }
}
