//! A producer's identity: the name and version of the software, its build,
//! and the hostname of the machine it runs on.
//!
//! A producer declares it in its stream handshake's metadata, and in a
//! snapshot's metadata, under the [`PRODUCER_NAME`], [`PRODUCER_VERSION`],
//! [`PRODUCER_BUILD`] and [`PRODUCER_HOST`] keys, one string each.
//! `metriken-exposition`'s `producer_identity!` fills the name and version
//! from the producer's crate and reads the hostname.

use std::collections::BTreeMap;

/// The metadata key naming the producer's software, such as `rezolus`.
pub const PRODUCER_NAME: &str = "metriken.producer.name";

/// The metadata key carrying the producer's version, such as `5.21.0`.
pub const PRODUCER_VERSION: &str = "metriken.producer.version";

/// The metadata key carrying the producer's build, such as a commit hash.
pub const PRODUCER_BUILD: &str = "metriken.producer.build";

/// The metadata key carrying the hostname of the machine the producer runs
/// on.
pub const PRODUCER_HOST: &str = "metriken.producer.host";

/// A producer's identity, as declared in metadata under the
/// `metriken.producer.*` keys.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProducerIdentity {
    /// The software's name.
    pub name: String,
    /// The software's version.
    pub version: Option<String>,
    /// The software's build, such as a commit hash.
    pub build: Option<String>,
    /// The hostname of the machine the producer runs on.
    pub host: Option<String>,
}

impl ProducerIdentity {
    /// An identity naming `name`, with no version, build or host.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: None,
            build: None,
            host: None,
        }
    }

    /// This identity with `version`.
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// This identity with `build`.
    pub fn with_build(mut self, build: impl Into<String>) -> Self {
        self.build = Some(build.into());
        self
    }

    /// This identity with `host`.
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = Some(host.into());
        self
    }

    /// The identity as metadata: [`PRODUCER_NAME`], and each of
    /// [`PRODUCER_VERSION`], [`PRODUCER_BUILD`] and [`PRODUCER_HOST`] that is
    /// set and not empty. Add it to a handshake's or a snapshot's metadata
    /// with `extend`.
    pub fn to_metadata(&self) -> BTreeMap<String, String> {
        let mut metadata = BTreeMap::from([(PRODUCER_NAME.to_string(), self.name.clone())]);
        for (key, value) in [
            (PRODUCER_VERSION, &self.version),
            (PRODUCER_BUILD, &self.build),
            (PRODUCER_HOST, &self.host),
        ] {
            if let Some(value) = value.as_ref().filter(|v| !v.is_empty()) {
                metadata.insert(key.to_string(), value.clone());
            }
        }
        metadata
    }

    /// The identity read from `metadata`, or `None` when it has no
    /// [`PRODUCER_NAME`] or an empty one. An empty version, build or host is
    /// read as absent. Other keys are ignored.
    pub fn from_metadata<'a>(
        metadata: impl IntoIterator<Item = (&'a String, &'a String)>,
    ) -> Option<Self> {
        let mut identity = Self::new(String::new());
        for (key, value) in metadata {
            let slot = match key.as_str() {
                PRODUCER_NAME => {
                    identity.name = value.clone();
                    continue;
                }
                PRODUCER_VERSION => &mut identity.version,
                PRODUCER_BUILD => &mut identity.build,
                PRODUCER_HOST => &mut identity.host,
                _ => continue,
            };
            *slot = Some(value.clone()).filter(|v| !v.is_empty());
        }
        (!identity.name.is_empty()).then_some(identity)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn an_identity_round_trips_through_metadata() {
        let identity = ProducerIdentity::new("rezolus")
            .with_version("5.21.0")
            .with_build("4d070d6")
            .with_host("db-1");
        let metadata = identity.to_metadata();
        assert_eq!(
            metadata,
            BTreeMap::from([
                (PRODUCER_NAME.to_string(), "rezolus".to_string()),
                (PRODUCER_VERSION.to_string(), "5.21.0".to_string()),
                (PRODUCER_BUILD.to_string(), "4d070d6".to_string()),
                (PRODUCER_HOST.to_string(), "db-1".to_string()),
            ])
        );
        assert_eq!(ProducerIdentity::from_metadata(&metadata), Some(identity));
    }

    #[test]
    fn unset_and_empty_fields_are_left_out() {
        let identity = ProducerIdentity::new("cachecannon").with_build("");
        assert_eq!(
            identity.to_metadata(),
            BTreeMap::from([(PRODUCER_NAME.to_string(), "cachecannon".to_string())])
        );
        let read = HashMap::from([
            (PRODUCER_NAME.to_string(), "cachecannon".to_string()),
            (PRODUCER_HOST.to_string(), String::new()),
        ]);
        assert_eq!(
            ProducerIdentity::from_metadata(&read),
            Some(ProducerIdentity::new("cachecannon"))
        );
    }

    #[test]
    fn metadata_without_a_name_reads_as_no_identity() {
        let other = BTreeMap::from([
            ("source".to_string(), "rezolus".to_string()),
            (PRODUCER_VERSION.to_string(), "5.21.0".to_string()),
        ]);
        assert_eq!(ProducerIdentity::from_metadata(&other), None);
        let empty = BTreeMap::from([(PRODUCER_NAME.to_string(), String::new())]);
        assert_eq!(ProducerIdentity::from_metadata(&empty), None);
    }
}
