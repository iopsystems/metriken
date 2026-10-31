//! Browser chart modules for metriken recordings, as files a server can embed
//! and serve.
//!
//! `assets/charts/` holds ES modules for the chart types, drawn with
//! [ECharts](https://echarts.apache.org) (line, multi-series, percentile
//! scatter, heatmaps and histogram heatmaps), and their helpers: zoom,
//! tooltips, event markers, and decoding the display-mode (decimated) bodies
//! that rezolus's `display_wire` encoder writes. `assets/charts.css` styles
//! the chart card. The modules import only from inside `assets/charts/`. What
//! a chart needs from its page (a data layer, a theme version, events) comes
//! through `charts/host.js`; see the README.
//!
//! - [`plot`]: the plot spec, the JSON a page gives a chart to say what it
//!   draws, built in Rust and serialized with serde.
//! - [`ASSETS`]: the `assets/` directory, embedded at build time.
//! - [`ASSETS_PATH`]: the directory Cargo built from, on the build machine.
//!   It is useful with a path dependency, to serve edits without a rebuild;
//!   in a deployed binary it usually does not exist, so serve [`ASSETS`].

pub mod plot;

pub use plot::{Plot, PlotOpts, PlotWidth, Unit};

use ::include_dir::Dir;
/// The `include_dir` version [`ASSETS`] is a [`Dir`] of, so a caller names the
/// same types.
pub use include_dir;

/// The `assets/` directory: `charts/` (the chart modules, with
/// `charts/echarts.min.js`) and `charts.css`.
pub static ASSETS: Dir<'static> = ::include_dir::include_dir!("$CARGO_MANIFEST_DIR/assets");

/// The path of the `assets/` directory in this crate's source on the machine
/// that built it: a workspace path, a git checkout or the registry cache.
pub const ASSETS_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/assets");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeds_the_chart_modules_and_stylesheet() {
        for path in [
            "charts/chart.js",
            "charts/host.js",
            "charts/display.js",
            "charts/echarts.min.js",
            "charts.css",
        ] {
            assert!(ASSETS.get_file(path).is_some(), "{path} is not embedded");
        }
    }

    #[test]
    fn assets_path_holds_the_embedded_files() {
        let on_disk = std::path::Path::new(ASSETS_PATH).join("charts/host.js");
        let embedded = ASSETS.get_file("charts/host.js").unwrap().contents();
        assert_eq!(std::fs::read(on_disk).unwrap(), embedded);
    }
}
