mod counter;
mod gauge;
mod histogram;
mod identity;
pub(crate) mod metadata;
pub(crate) mod windows;

pub use counter::CounterGroup;
pub use gauge::GaugeGroup;
pub use histogram::HistogramGroup;
pub use identity::{SlotIdentity, SlotMetadata, UID_LABEL};
