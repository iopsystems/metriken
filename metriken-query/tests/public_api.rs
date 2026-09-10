//! Tests that exercise the public API as a DOWNSTREAM crate sees it.
//!
//! Deliberately not in `integration.rs`, which is gated behind the
//! `fixtures` feature and so is skipped by a default `cargo test`. These
//! assert things an in-crate unit test structurally cannot: `#[non_exhaustive]`
//! and private fields do not apply within the defining crate, so only an
//! external crate can prove a type is still constructible.

/// `EnvPoint` is constructible from OUTSIDE the crate.
///
/// This lives here rather than in the unit tests because `#[non_exhaustive]`
/// does not apply within the defining crate — an in-crate test would pass even
/// if the constructors were missing or private, which is precisely the mistake
/// this guards. Marking the struct non-exhaustive removed literal construction
/// for downstreams, so `new` + the `with_*` builders have to be enough on their
/// own, and only an integration test proves it.
#[test]
fn env_point_is_constructible_by_a_downstream() {
    use metriken_query::EnvPoint;

    let plain = EnvPoint::new(1.0, 2.0, 3.0, 4.0, 5.0, 6.0);
    assert_eq!(plain.median, 4.0);
    assert_eq!((plain.unc_lo, plain.unc_hi), (None, None));
    assert!(!plain.interpolated);

    let full = EnvPoint::new(1.0, 2.0, 3.0, 4.0, 5.0, 6.0)
        .with_band(Some((3.5, 4.5)))
        .with_interpolated(true);
    assert_eq!((full.unc_lo, full.unc_hi), (Some(3.5), Some(4.5)));
    assert!(full.interpolated);

    // Clearing the band puts both edges back to None together — they are
    // documented as parallel, so neither can be set without the other.
    assert_eq!(
        {
            let p = full.with_band(None);
            (p.unc_lo, p.unc_hi)
        },
        (None, None),
    );
}
