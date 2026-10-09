//! `metriken::Window` is the type `metriken-types` defines, and
//! `metriken::group::UID_LABEL` has its value.

#[test]
fn window_is_the_metriken_types_window() {
    fn same(w: metriken_types::Window) -> metriken::Window {
        w
    }
    let w = metriken_types::Window::new(1, 3);
    assert_eq!(same(w).width_ns(), 2);
}

#[test]
fn uid_label_is_the_metriken_types_label() {
    assert_eq!(metriken::group::UID_LABEL, metriken_types::UID_LABEL);
}
