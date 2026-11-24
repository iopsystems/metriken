mod counter;
mod gauge;
mod histogram;
pub(crate) mod identity;
pub(crate) mod metadata;
mod rwlock_histogram;
pub(crate) mod windows;

pub use counter::CounterGroup;
pub use gauge::GaugeGroup;
pub use histogram::CounterHistogramGroup;
pub use rwlock_histogram::RwLockCounterHistogramGroup;

/// The former name of [`CounterHistogramGroup`].
#[deprecated(
    since = "0.11.4",
    note = "renamed `CounterHistogramGroup`, the counting kind beside `GaugeHistogram`"
)]
pub type HistogramGroup = CounterHistogramGroup;
pub use identity::{SlotIdentity, SlotMetadata, UID_LABEL};
