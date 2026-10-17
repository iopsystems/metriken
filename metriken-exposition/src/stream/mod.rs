//! A producer's groups as dendro replication frames, and the route that
//! serves them (feature `stream`).
//!
//! A producer is one dendro *source*: one clock domain, one identity, one
//! sequence of observations. A subscription is dendro's preamble, a
//! [`Frame::Handshake`] once, then one [`Frame::Rows`] per interval whose
//! rows are encoded [`WalGroupRow`]s, [`WalLongRow`]s, and [`Occupant`] rows
//! on `<group>/occupants`.
//!
//! # Serving `/metrics/stream`
//!
//! A producer mounts the route in its own HTTP server. It parses the query
//! string with [`StreamRequest::parse`] (a [`Refusal`] carries the status to
//! answer with), builds a [`StreamSubscription`], sends its
//! [`headers`](StreamSubscription::headers), then the
//! [`opening`](StreamSubscription::opening) and, at each
//! [`next_deadline`](StreamSubscription::next_deadline), the
//! [`frame_at`](StreamSubscription::frame_at) for the pass it fetched. With
//! feature `stream-tokio`, `StreamSubscription::into_stream` does the
//! timing and fetching as a body stream. A producer whose metrics are a
//! metriken registry fetches from [`RegistryPasses`]; one with its own pass
//! cache implements [`Pass`] for its pass type.
//!
//! Each subscription has its own timer, aligned to its interval since the
//! Unix epoch, and `seq` is the index of that interval.
//!
//! # The interval can be negotiated
//!
//! The producer's update floor bounds how often it can have anything new to
//! send. The request offers an interval. A subscription built with
//! [`negotiated`](StreamSubscription::negotiated) is served at the larger of
//! the offer and the floor, and names both in the frame-interval and
//! update-floor headers and in the handshake's metadata under
//! [`HANDSHAKE_FRAME_INTERVAL`] and [`HANDSHAKE_UPDATE_FLOOR`]. Without it
//! the offer is served as given, and only the headers name the interval and
//! the floor.
//!
//! - A subscriber MUST take the served interval as the stream's interval,
//!   and MUST offer at least the floor once it knows it (a reconnect).
//! - A producer MAY refuse an offer below its floor, with a 400 naming the
//!   floor, or MAY serve it at the floor. A
//!   [`negotiated`](StreamSubscription::negotiated) subscription serves it at
//!   the floor; a producer that refuses checks the offer against its floor
//!   before building one.
//!
//! The two handshake keys are per connection: a subscriber that keeps a
//! source's handshake metadata drops them.
//!
//! # No identity index
//!
//! No [`Frame::Index`] is sent. For a group sent wide, what a slot means
//! travels in the group's schema (each member's labels, `__uid__` included),
//! which the payload carries whenever it changes. For a group sent long, it
//! travels on `<group>/occupants` (see below). Every rows frame names dendro's
//! [`NO_INDEX_STATE`], which a subscriber always resolves.
//!
//! # Time is the producer's
//!
//! Rows are stamped by the producer, because only the producer knows when
//! the values were read: a pass may be cached and served later, and on a
//! stream there is no consumer request at all, only the send. dendro's
//! FORMAT.md §5 makes one source one clock domain, and the anchor that pins
//! the timeline (`clock_anchor_wall_ns`) is a handshake field. If a
//! subscriber stamped, two subscribers to one producer would build two
//! timelines for one source, which could not be merged.
//!
//! So `ts` is anchored (`anchor + monotonic elapsed`, [`metriken::epoch`]):
//! wall-clock magnitude, strictly increasing through a clock step.
//! `wall_offset` is wall minus `ts` at the read, so `ts + wall_offset` is the
//! wall clock and a step is visible rather than absorbed. The anchor is one
//! per process, so every subscription is told the same timeline.
//!
//! # The schema travels inside the payload
//!
//! A [`WalRow`] has no envelope: `stream`, `ts`, `wall_offset` and an opaque
//! payload. A group's schema therefore goes inside the payload, in
//! [`WalGroupRow::schema`], on the first row of a stream and whenever the
//! schema's hash changes, and is left out otherwise.
//!
//! # Groups of slotted metrics are sent long
//!
//! A [`LongGroupSnapshot`] is sent as a [`WalLongRow`] on the group's stream,
//! keyed by the producer's occupant keys, whose schema is the group's metric
//! columns and changes only when its metrics do. Before it, on
//! `<group>/occupants`, the subscription is sent an [`Occupant`] (key and
//! labels) for each occupant that was not in the previous row of this group
//! sent to this subscription: new occupants, and every occupant on the
//! subscription's first row. A departure is not sent; the occupant is absent
//! from later rows. A group can change form between rows; the subscriber
//! reads the form from each row, and a change of form sends the row's schema
//! and, for a long row, every occupant again.
//!
//! [`Frame::Handshake`]: dendro::replicate::Frame::Handshake
//! [`Frame::Rows`]: dendro::replicate::Frame::Rows
//! [`Frame::Index`]: dendro::replicate::Frame::Index
//! [`NO_INDEX_STATE`]: dendro::replicate::NO_INDEX_STATE
//! [`WalRow`]: dendro::archive::WalRow
//! [`WalGroupRow`]: metriken_model::wal::WalGroupRow
//! [`WalGroupRow::schema`]: metriken_model::wal::WalGroupRow::schema
//! [`WalLongRow`]: metriken_model::wal::WalLongRow
//! [`Occupant`]: metriken_model::occupants::Occupant
//! [`LongGroupSnapshot`]: crate::group_builder::LongGroupSnapshot

#[cfg(feature = "stream-tokio")]
mod body;
mod clock;
mod frames;
mod route;

pub use frames::{
    encode_groups, EncodedGroup, EncodedLongGroup, EncodedStreamGroup, FrameProducer, KeyedLabels,
    SchemaCache, StreamRow, SOURCE,
};
// The wire names and layouts are `metriken-model`'s, so a consumer can share
// them without the registry; re-exported here for producers.
pub use metriken_model::wire::{
    Layout, WireNames, HANDSHAKE_FRAME_INTERVAL, HANDSHAKE_UPDATE_FLOOR, WIRE_NAMES,
};
pub use route::{
    Pass, PassStamp, Refusal, RegistryPass, RegistryPasses, StreamRequest, StreamSubscription,
};
