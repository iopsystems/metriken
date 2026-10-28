//! The name a source is shown under in events, logs and the summary.
//!
//! The recorder writes no `source` of its own: a source's labels and
//! metadata are its identity layers (`crate::layered`). The name shown is the
//! caller's `source` label, else the handshake's, else the probe hook's (a
//! hook sets it when it recognises the producer), else one inferred from the
//! URL.

#[cfg(feature = "http")]
use std::collections::BTreeMap;

/// Where a source's shown name can come from.
#[cfg(feature = "http")]
pub(crate) struct Identity<'a> {
    /// The caller's labels, `source` included when the caller named one.
    pub user_labels: &'a BTreeMap<String, String>,
    pub hook_labels: &'a BTreeMap<String, String>,
    pub handshake_labels: &'a BTreeMap<String, String>,
    /// The name to use when nobody named the source.
    pub inferred: &'a str,
}

#[cfg(feature = "http")]
impl Identity<'_> {
    /// The name shown for the source, and whether it is the inferred name.
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
}

/// A source name from a URL: host, port, and the path when it says more than
/// `/metrics`, as one word (`svc-9090`, `svc-9090-federate`). The recorder
/// shows it for a source nobody named and does not write it.
///
/// The path is kept because several exporters behind one address,
/// distinguished only by path, is an ordinary Prometheus deployment, and two
/// targets showing the same name could not be told apart. `/metrics` is left
/// out because it is the convention, not a distinction.
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
    // One plain word, so a caller can use it as a label value: anything but
    // ASCII alphanumerics becomes `-`, runs collapse, and the edges are
    // trimmed.
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

    /// The shown name: the caller's `source`, the handshake's, the hook's,
    /// the inferred one.
    #[cfg(feature = "http")]
    #[test]
    fn the_shown_name_takes_the_caller_then_the_handshake_then_the_hook() {
        let empty = BTreeMap::new();
        let hook = map(&[("source", "rezolus")]);
        let handshake = map(&[("source", "cachecannon")]);
        let mine = map(&[("source", "mine")]);
        let id = Identity {
            user_labels: &mine,
            hook_labels: &hook,
            handshake_labels: &handshake,
            inferred: "web-01-4241",
        };
        assert_eq!(id.source(), ("mine".to_string(), false));
        let id = Identity {
            user_labels: &empty,
            ..id
        };
        assert_eq!(id.source(), ("cachecannon".to_string(), false));
        let id = Identity {
            handshake_labels: &empty,
            ..id
        };
        assert_eq!(id.source(), ("rezolus".to_string(), false));
        let id = Identity {
            hook_labels: &empty,
            ..id
        };
        assert_eq!(id.source(), ("web-01-4241".to_string(), true));
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
