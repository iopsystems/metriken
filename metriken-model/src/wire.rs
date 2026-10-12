//! The names a producer's `/metrics/stream` is served under, and the layouts
//! it can send, shared by the producer (`metriken-exposition`'s `stream`) and
//! a consumer that decodes the stream without the registry.

use std::fmt;

/// How a subscription's groups are sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Layout {
    /// Every group as a [`WalGroupRow`](crate::wal::WalGroupRow).
    #[default]
    Wide,
    /// A group whose metrics are all counter groups, gauge groups or
    /// histogram groups as a [`WalLongRow`](crate::wal::WalLongRow) keyed by
    /// occupant, after the occupants it introduces; any other group wide.
    Long,
}

impl Layout {
    /// `"wide"` or `"long"`: the value of the `layout` query parameter and
    /// of the layout header.
    pub fn as_str(&self) -> &'static str {
        match self {
            Layout::Wide => "wide",
            Layout::Long => "long",
        }
    }
}

impl fmt::Display for Layout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The names a stream is served under: its `Content-Type` and its headers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WireNames {
    pub content_type: &'static str,
    /// How often a frame is sent: the request's interval.
    pub frame_interval: &'static str,
    /// How often a frame can carry anything new: the producer's freshness
    /// floor.
    pub update_floor: &'static str,
    /// The layout served.
    pub layout: &'static str,
}

/// The names a metriken producer serves.
pub const WIRE_NAMES: WireNames = WireNames {
    content_type: "application/vnd.metriken.replication.v1+dendro",
    frame_interval: "x-metriken-frame-interval",
    update_floor: "x-metriken-update-floor",
    layout: "x-metriken-layout",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_layout_names_itself_as_the_query_parameter_does() {
        assert_eq!(Layout::default(), Layout::Wide);
        assert_eq!(Layout::Wide.as_str(), "wide");
        assert_eq!(Layout::Long.as_str(), "long");
        assert_eq!(Layout::Long.to_string(), "long");
    }

    #[test]
    fn the_wire_names_are_metrikens() {
        assert_eq!(
            WIRE_NAMES.content_type,
            "application/vnd.metriken.replication.v1+dendro"
        );
        assert_eq!(WIRE_NAMES.frame_interval, "x-metriken-frame-interval");
        assert_eq!(WIRE_NAMES.update_floor, "x-metriken-update-floor");
        assert_eq!(WIRE_NAMES.layout, "x-metriken-layout");
    }
}
