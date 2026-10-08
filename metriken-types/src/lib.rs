//! Types shared by the metriken registry and the code that reads what it
//! records.
//!
//! The registry (`metriken-core`, `metriken`) and the readers (the data
//! model, storage, the query engine, the viewer) both name these, so they
//! live below `metriken-core`. Nothing here depends on the registry, so a
//! reader can use them without linking it: `metriken-core` declares
//! `links = "metriken-core"` and a `linkme` distributed slice, which has no
//! wasm32 implementation.
//!
//! - [`Window`]: the interval over which a value was read.
//! - [`UID_LABEL`]: the label that tells one occupant of a slot from
//!   another.
//!
//! A type belongs here only if both sides use it and it is not expected to
//! change, since a breaking release of this crate is a breaking release of
//! `metriken-core`.

mod window;

pub use window::Window;

/// The label that names one occupant of a slot.
///
/// A slot's labels say what it means (`comm=redis pid=4112`), and two
/// different occupants can share them: a PID wraps, a cgroup is deleted and
/// recreated at the same path, a task restarts under the same name. The uid
/// is minted once per assignment and travels with the labels, so two
/// consumers of one process see the same uid for the same occupant without
/// coordinating.
///
/// Internal under the `__` rule: part of a series' identity and matchable,
/// hidden from listings and legends.
pub const UID_LABEL: &str = "__uid__";
