//! `producer_identity!` expanded in a package other than metriken-exposition
//! names that package, since `CARGO_PKG_NAME` and `CARGO_PKG_VERSION` are read
//! where the macro expands.

#[test]
fn the_macro_names_the_crate_it_expands_in() {
    let identity = metriken_exposition::producer_identity!();
    assert_eq!(identity.name, "metriken-recorder");
    assert_eq!(identity.version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
}
