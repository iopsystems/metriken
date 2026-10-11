//! The segment format's version.
//!
//! Every segment this crate writes carries [`FORMAT_KEY`] = [`FORMAT_VERSION`]
//! in its file key-value metadata. A reader calls [`check`] on a segment's
//! key-value metadata before interpreting its columns, and refuses one it
//! cannot read rather than misreading it:
//!
//! - a [`FORMAT_KEY`] newer than [`FORMAT_VERSION`], or one that is not a
//!   number;
//! - a [`LAYOUT_KEY`] other than [`LAYOUT_LONG`]. Without this check a reader
//!   takes any segment not marked long for a wide one, so a new layout would
//!   read as a wide table with the wrong columns.
//!
//! A segment without [`FORMAT_KEY`] is format 1: every segment written before
//! the key existed, and any parquet file that is not a segment at all.
//!
//! Change [`FORMAT_VERSION`] when a reader of the previous version would
//! misread a segment the new writer produces. Adding a column or a key that
//! old readers ignore does not need it.

use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;

use crate::long::{LAYOUT_KEY, LAYOUT_LONG};

/// File key-value metadata key holding the segment format version.
pub const FORMAT_KEY: &str = "metriken.format";

/// The format version this crate writes, and the newest it reads.
pub const FORMAT_VERSION: u32 = 1;

/// Why a segment was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatError(String);

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FormatError {}

impl From<FormatError> for String {
    fn from(e: FormatError) -> String {
        e.0
    }
}

/// Refuse a segment this reader cannot interpret; see the module docs.
pub fn check(kv: Option<&Vec<KeyValue>>) -> Result<(), FormatError> {
    let get = |key: &str| {
        kv.and_then(|kv| kv.iter().find(|e| e.key == key))
            .map(|e| e.value.as_deref().unwrap_or(""))
    };
    if let Some(v) = get(FORMAT_KEY) {
        match v.parse::<u32>() {
            Ok(n) if n <= FORMAT_VERSION => {}
            Ok(n) => {
                return Err(FormatError(format!(
                    "segment format {n} is newer than this reader, which reads up to \
                     {FORMAT_VERSION}; read it with a newer release"
                )))
            }
            Err(_) => {
                return Err(FormatError(format!(
                    "segment format {v:?} is not a version number"
                )))
            }
        }
    }
    if let Some(layout) = get(LAYOUT_KEY) {
        if layout != LAYOUT_LONG {
            return Err(FormatError(format!(
                "segment layout {layout:?} is not one this reader knows; read it with a \
                 newer release"
            )));
        }
    }
    Ok(())
}

/// `props` with [`FORMAT_KEY`] added to its key-value metadata, keeping any
/// the caller set.
pub fn stamped(props: WriterProperties) -> WriterProperties {
    let mut kv = props.key_value_metadata().cloned().unwrap_or_default();
    kv.retain(|e| e.key != FORMAT_KEY);
    kv.push(KeyValue::new(
        FORMAT_KEY.to_string(),
        FORMAT_VERSION.to_string(),
    ));
    props
        .into_builder()
        .set_key_value_metadata(Some(kv))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(pairs: &[(&str, &str)]) -> Vec<KeyValue> {
        pairs
            .iter()
            .map(|(k, v)| KeyValue::new(k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn an_unmarked_file_and_this_version_are_read() {
        assert_eq!(check(None), Ok(()));
        assert_eq!(check(Some(&kv(&[("source", "rezolus")]))), Ok(()));
        assert_eq!(check(Some(&kv(&[(FORMAT_KEY, "1")]))), Ok(()));
        assert_eq!(
            check(Some(&kv(&[(FORMAT_KEY, "1"), (LAYOUT_KEY, LAYOUT_LONG)]))),
            Ok(())
        );
    }

    #[test]
    fn a_newer_format_or_an_unknown_layout_is_refused() {
        let e = check(Some(&kv(&[(FORMAT_KEY, "2")]))).unwrap_err();
        assert!(e.to_string().contains("segment format 2 is newer"), "{e}");
        let e = check(Some(&kv(&[(FORMAT_KEY, "two")]))).unwrap_err();
        assert!(e.to_string().contains("not a version number"), "{e}");
        let e = check(Some(&kv(&[(LAYOUT_KEY, "long2")]))).unwrap_err();
        assert!(e.to_string().contains("\"long2\""), "{e}");
    }

    #[test]
    fn stamping_keeps_the_callers_metadata() {
        let props = WriterProperties::builder()
            .set_key_value_metadata(Some(kv(&[("a", "b"), (FORMAT_KEY, "0")])))
            .build();
        let got = stamped(props).key_value_metadata().cloned().unwrap();
        assert_eq!(got, kv(&[("a", "b"), (FORMAT_KEY, "1")]));
    }
}
