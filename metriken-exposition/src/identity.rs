//! The producer's identity, filled from the producer's crate and the machine
//! it runs on.

/// The hostname of the machine this process runs on, as the OS reports it
/// (`gethostname` on unix), neither shortened nor resolved, so it may be
/// fully qualified. `None` when it cannot be read or is empty. A name that
/// is not UTF-8 is converted lossily.
pub fn hostname() -> Option<String> {
    #[cfg(any(unix, windows))]
    {
        let name = ::hostname::get().ok()?;
        let name = name.to_string_lossy().into_owned();
        (!name.is_empty()).then_some(name)
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// The calling crate's [`ProducerIdentity`](crate::ProducerIdentity): its
/// package name and version (`CARGO_PKG_NAME`, `CARGO_PKG_VERSION`, read where
/// the macro expands), and the [`hostname()`] read at run time. Set `build` on
/// the result where the producer has one.
///
/// ```
/// let identity = metriken_exposition::producer_identity!();
/// assert_eq!(identity.name, env!("CARGO_PKG_NAME"));
/// assert_eq!(identity.version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
///
/// // Into a handshake's or a snapshot's metadata.
/// let mut metadata = std::collections::BTreeMap::new();
/// metadata.extend(identity.to_metadata());
/// ```
#[macro_export]
macro_rules! producer_identity {
    () => {{
        let identity = $crate::ProducerIdentity::new(::core::env!("CARGO_PKG_NAME"))
            .with_version(::core::env!("CARGO_PKG_VERSION"));
        match $crate::hostname() {
            ::core::option::Option::Some(host) => identity.with_host(host),
            ::core::option::Option::None => identity,
        }
    }};
}

#[cfg(test)]
mod tests {
    // That the name and version are the calling crate's is tested from
    // another package, metriken-recorder's `tests/producer_identity.rs`.
    #[test]
    fn the_macro_sets_no_build_and_reads_the_hostname() {
        let identity = crate::producer_identity!();
        assert_eq!(identity.build, None);
        assert_eq!(identity.host, super::hostname());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn a_host_has_a_hostname() {
        assert!(super::hostname().is_some_and(|h| !h.is_empty()));
    }
}
