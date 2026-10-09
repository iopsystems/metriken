//! Types the metriken registry writes and that a reader needs to interpret
//! what it records.
//!
//! Nothing here depends on the registry, so a reader can use these types
//! without linking it: `metriken-core` declares `links = "metriken-core"` and
//! a `linkme` distributed slice, which has no wasm32 implementation.
//! `metriken-core` re-exports [`Window`] and `metriken` re-exports
//! [`UID_LABEL`] at their existing paths.
//!
//! - [`Window`]: the interval over which a value was read.
//! - [`UID_LABEL`]: the label that tells one occupant of a slot from
//!   another.
//!
//! A type belongs here only if both the registry and readers use it and it is
//! not expected to change: `metriken-core` and `metriken` re-export these
//! items, so moving either of them to a new major version of this crate is a
//! breaking release of both.

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
/// A label whose key starts with `__` is internal: part of a series'
/// identity and matchable, hidden from listings and legends.
pub const UID_LABEL: &str = "__uid__";
