//! Whose word a recording's labels and metadata take.
//!
//! Labels, highest first: the caller's, the probe hook's, the stream
//! handshake's. `source` is ordered on its own: the caller's, the
//! handshake's, the hook's (a hook sets it when it recognises the producer),
//! a name inferred from the URL. Metadata, highest first: the caller's, the
//! recorder's own keys, the hook's, the handshake's.

#[cfg(feature = "http")]
use std::collections::BTreeMap;

/// The identity a recording is added with, from the places its labels and
/// metadata can come from.
#[cfg(feature = "http")]
pub(crate) struct Identity<'a> {
    /// The caller's labels, `source` included when the caller named one.
    pub user_labels: &'a BTreeMap<String, String>,
    pub user_metadata: &'a BTreeMap<String, String>,
    /// Keys the recorder writes itself (`sampling_interval_ms`,
    /// `producer_epoch`).
    pub recorder_metadata: BTreeMap<String, String>,
    pub hook_labels: &'a BTreeMap<String, String>,
    pub hook_metadata: &'a BTreeMap<String, String>,
    pub handshake_labels: &'a BTreeMap<String, String>,
    pub handshake_metadata: &'a BTreeMap<String, String>,
    /// The name to use when nobody named the source.
    pub inferred: &'a str,
}

#[cfg(feature = "http")]
impl Identity<'_> {
    /// The recording's `source`, and whether it is the inferred name.
    pub(crate) fn source(&self) -> (String, bool) {
        match self
            .user_labels
            .get(crate::keys::SOURCE)
            .or_else(|| self.handshake_labels.get(crate::keys::SOURCE))
            .or_else(|| self.hook_labels.get(crate::keys::SOURCE))
        {
            Some(named) => (named.clone(), false),
            None => (self.inferred.to_string(), true),
        }
    }

    /// Labels: the handshake's, under the hook's, under the caller's, with
    /// `source` from [`source`](Self::source).
    pub(crate) fn labels(&self) -> BTreeMap<String, String> {
        let mut labels = self.handshake_labels.clone();
        labels.extend(self.hook_labels.clone());
        labels.insert(crate::keys::SOURCE.to_string(), self.source().0);
        labels.extend(self.user_labels.clone());
        labels
    }

    /// Metadata: the handshake's, under the hook's, under `source` and the
    /// recorder's keys, under the caller's.
    pub(crate) fn metadata(&self) -> BTreeMap<String, String> {
        let mut metadata = self.handshake_metadata.clone();
        metadata.extend(self.hook_metadata.clone());
        metadata.insert(crate::keys::SOURCE.to_string(), self.source().0);
        metadata.extend(self.recorder_metadata.clone());
        metadata.extend(self.user_metadata.clone());
        metadata
    }
}

/// A source name from a URL: host, port, and the path when it says more than
/// `/metrics`, as one word (`svc-9090`, `svc-9090-federate`).
///
/// The path is kept because several exporters behind one address,
/// distinguished only by path, is an ordinary Prometheus deployment, and two
/// targets inferring the same name get identical label sets that no reader
/// can tell apart. `/metrics` is left out because it is the convention, not
/// a distinction.
pub fn infer_source_name(host: Option<&str>, port: Option<u16>, path: &str) -> String {
    let host = host.unwrap_or("unknown");
    let port = port.map(|p| format!("-{p}")).unwrap_or_default();
    match distinguishing_path(path) {
        Some(path) => format!("{host}{port}-{path}"),
        None => format!("{host}{port}"),
    }
}

fn distinguishing_path(path: &str) -> Option<String> {
    let path = path.trim_matches('/');
    if path.is_empty() || path == "metrics" {
        return None;
    }
    // The name becomes a `source` label value, which selectors match, so it
    // is one plain word: anything but ASCII alphanumerics becomes `-`, runs
    // collapse, and the edges are trimmed.
    let slug: String = path
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    (!slug.is_empty()).then_some(slug)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "http")]
    use std::collections::BTreeMap;

    #[cfg(feature = "http")]
    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Labels: caller over hook over handshake. `source`: caller,
    /// handshake, hook, inferred.
    #[cfg(feature = "http")]
    #[test]
    fn labels_take_the_caller_then_the_hook_then_the_handshake() {
        let hook = map(&[
            ("source", "rezolus"),
            ("host", "from-hook"),
            ("arm", "hook"),
        ]);
        let handshake = map(&[
            ("source", "cachecannon"),
            ("host", "from-handshake"),
            ("zone", "a"),
        ]);
        let empty = BTreeMap::new();
        let user = map(&[("arm", "user")]);
        let id = Identity {
            user_labels: &user,
            user_metadata: &empty,
            recorder_metadata: BTreeMap::new(),
            hook_labels: &hook,
            hook_metadata: &empty,
            handshake_labels: &handshake,
            handshake_metadata: &empty,
            inferred: "web-01-4241",
        };
        assert_eq!(
            id.labels(),
            map(&[
                ("source", "cachecannon"),
                ("host", "from-hook"),
                ("arm", "user"),
                ("zone", "a"),
            ])
        );

        let mine = map(&[("source", "mine")]);
        let id = Identity {
            user_labels: &mine,
            ..id
        };
        assert_eq!(id.source(), ("mine".to_string(), false));

        let no_handshake_source = map(&[("zone", "a")]);
        let id = Identity {
            user_labels: &empty,
            handshake_labels: &no_handshake_source,
            ..id
        };
        assert_eq!(
            id.source(),
            ("rezolus".to_string(), false),
            "the hook's, when the handshake names none"
        );

        let id = Identity {
            hook_labels: &empty,
            ..id
        };
        assert_eq!(id.source(), ("web-01-4241".to_string(), true));
    }

    #[cfg(feature = "http")]
    #[test]
    fn metadata_takes_the_caller_then_the_recorder_then_the_hook_then_the_handshake() {
        let hook = map(&[("version", "hook"), ("systeminfo", "{}")]);
        let handshake = map(&[
            ("version", "handshake"),
            ("producer_epoch", "handshake"),
            ("build", "abc"),
        ]);
        let empty = BTreeMap::new();
        let user = map(&[("systeminfo", "user")]);
        let id = Identity {
            user_labels: &empty,
            user_metadata: &user,
            recorder_metadata: map(&[("producer_epoch", "recorder")]),
            hook_labels: &empty,
            hook_metadata: &hook,
            handshake_labels: &empty,
            handshake_metadata: &handshake,
            inferred: "svc-8080",
        };
        let m = id.metadata();
        assert_eq!(m["version"], "hook");
        assert_eq!(m["producer_epoch"], "recorder");
        assert_eq!(m["systeminfo"], "user");
        assert_eq!(m["build"], "abc");
        assert_eq!(m["source"], "svc-8080");
    }

    #[test]
    fn an_inferred_name_keeps_a_distinguishing_path_as_one_word() {
        for (path, name) in [
            ("/metrics", "svc-9090"),
            ("/metrics/", "svc-9090"),
            ("/", "svc-9090"),
            ("", "svc-9090"),
            ("/federate", "svc-9090-federate"),
            ("/metrics//v2/tenant_a/", "svc-9090-metrics-v2-tenant-a"),
        ] {
            assert_eq!(infer_source_name(Some("svc"), Some(9090), path), name);
        }
        assert_eq!(
            infer_source_name(Some("example.com"), None, "/metrics"),
            "example.com"
        );
    }
}
