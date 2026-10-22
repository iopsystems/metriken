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
//!
//! The versions:
//!
//! - 1: counters, gauges and histograms.
//! - 2 ([`FORMAT_GAUGE_HISTOGRAM`]): a segment holding a gauge histogram
//!   column (`metric_type = gauge_histogram`). Its buckets are laid out as a
//!   histogram's, and a format 1 reader, which classifies a list column as a
//!   histogram by its Arrow type alone, would difference its rows. A writer
//!   stamps 2 only on a segment that holds one ([`format_of`]), so an archive
//!   without gauge histograms stays readable by format 1 readers.

use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;

use crate::long::{LAYOUT_KEY, LAYOUT_LONG};

/// File key-value metadata key holding the segment format version.
pub const FORMAT_KEY: &str = "metriken.format";

/// The newest format version this crate reads, and the one it writes for a
/// segment holding a gauge histogram. Other segments are written as format 1
/// ([`format_of`]).
pub const FORMAT_VERSION: u32 = 2;

/// The format of a segment holding a gauge histogram column.
pub const FORMAT_GAUGE_HISTOGRAM: u32 = 2;

/// The format a segment with `schema`'s columns is written as:
/// [`FORMAT_GAUGE_HISTOGRAM`] when a column's field metadata has
/// `metric_type = gauge_histogram`, else 1.
pub fn format_of(schema: &arrow::datatypes::Schema) -> u32 {
    let gauge_histogram = schema.fields().iter().any(|f| {
        f.metadata()
            .get(metriken_model::METRIC_TYPE)
            .map(String::as_str)
            == Some(metriken_model::GAUGE_HISTOGRAM)
    });
    if gauge_histogram {
        FORMAT_GAUGE_HISTOGRAM
    } else {
        1
    }
}

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

/// `props` with [`FORMAT_KEY`] = 1 added to its key-value metadata, keeping
/// any the caller set: for a file with no gauge histogram column. A segment
/// writer stamps [`stamped_for`] its schema instead.
pub fn stamped(props: WriterProperties) -> WriterProperties {
    stamped_as(props, 1)
}

/// `props` stamped with the format a segment of `schema` is
/// ([`format_of`]).
pub fn stamped_for(props: WriterProperties, schema: &arrow::datatypes::Schema) -> WriterProperties {
    stamped_as(props, format_of(schema))
}

/// `props` stamped as [`stamped_for`] does when `schema` needs a format newer
/// than 1, and unchanged otherwise: for a writer whose format 1 files have
/// always been unmarked.
pub fn stamped_if_newer(
    props: WriterProperties,
    schema: &arrow::datatypes::Schema,
) -> WriterProperties {
    match format_of(schema) {
        1 => props,
        version => stamped_as(props, version),
    }
}

fn stamped_as(props: WriterProperties, version: u32) -> WriterProperties {
    let mut kv = props.key_value_metadata().cloned().unwrap_or_default();
    kv.retain(|e| e.key != FORMAT_KEY);
    kv.push(KeyValue::new(FORMAT_KEY.to_string(), version.to_string()));
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
        assert_eq!(check(Some(&kv(&[(FORMAT_KEY, "2")]))), Ok(()));
        assert_eq!(
            check(Some(&kv(&[(FORMAT_KEY, "1"), (LAYOUT_KEY, LAYOUT_LONG)]))),
            Ok(())
        );
    }

    #[test]
    fn a_newer_format_or_an_unknown_layout_is_refused() {
        let e = check(Some(&kv(&[(FORMAT_KEY, "3")]))).unwrap_err();
        assert!(e.to_string().contains("segment format 3 is newer"), "{e}");
        let e = check(Some(&kv(&[(FORMAT_KEY, "two")]))).unwrap_err();
        assert!(e.to_string().contains("not a version number"), "{e}");
        let e = check(Some(&kv(&[(LAYOUT_KEY, "long2")]))).unwrap_err();
        assert!(e.to_string().contains("\"long2\""), "{e}");
    }

    /// A schema with a gauge histogram column is format 2; one with
    /// counting histograms only, or a `metric_type` of another kind, is 1.
    #[test]
    fn a_gauge_histogram_column_makes_the_segment_format_2() {
        use arrow::datatypes::{DataType, Field, Schema};
        let field = |kind: &str| {
            Field::new(
                "x:buckets",
                DataType::new_list(DataType::UInt64, true),
                true,
            )
            .with_metadata(std::collections::HashMap::from([(
                "metric_type".to_string(),
                kind.to_string(),
            )]))
        };
        let ts = Field::new("timestamp", DataType::UInt64, false);
        let counting = Schema::new(vec![ts.clone(), field("histogram")]);
        let gauge = Schema::new(vec![ts, field("histogram"), field("gauge_histogram")]);
        assert_eq!(format_of(&counting), 1);
        assert_eq!(format_of(&gauge), FORMAT_GAUGE_HISTOGRAM);
        let stamped = stamped_for(WriterProperties::builder().build(), &gauge);
        assert_eq!(
            stamped.key_value_metadata().cloned().unwrap(),
            kv(&[(FORMAT_KEY, "2")])
        );
        // This reader reads it. A reader of format 1 (metriken-storage 0.3.0
        // to 0.4.x) refuses it in its `check`; that is verified against the
        // published crates, see the journal entry's "As built".
        assert_eq!(check(stamped.key_value_metadata()), Ok(()));
        assert_eq!(
            stamped_if_newer(WriterProperties::builder().build(), &counting).key_value_metadata(),
            None,
            "a file without one is left unmarked"
        );
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
