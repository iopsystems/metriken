use metriken::*;

#[metric]
static MODULE_PATH_TEST_METRIC: Counter = Counter::new();

#[test]
fn metric_records_its_definition_module() {
    let metrics = metrics().static_metrics();
    let entry = metrics
        .iter()
        .find(|entry| entry.is(&MODULE_PATH_TEST_METRIC))
        .expect("metric registered");
    assert_eq!(entry.module(), module_path!());
}
