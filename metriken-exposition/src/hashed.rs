//! The parquet conversion's view of a snapshot: its readings keyed by name.
//! The snapshot types themselves are `metriken-model`'s.

use std::collections::HashMap;
use std::time::SystemTime;

use crate::{Counter, Gauge, Histogram, Snapshot};

pub(crate) struct HashedSnapshot {
    pub(crate) ts: u64,
    pub(crate) duration: Option<u64>,
    pub(crate) counters: HashMap<String, Counter>,
    pub(crate) gauges: HashMap<String, Gauge>,
    pub(crate) histograms: HashMap<String, Histogram>,
}

impl From<Snapshot> for HashedSnapshot {
    fn from(mut snapshot: Snapshot) -> Self {
        let ts: u64 = snapshot
            .systemtime()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("System Clock is earlier than 1970; needs reset")
            .as_nanos() as u64;

        let duration: Option<u64> = snapshot.duration().map(|x| x.as_nanos() as u64);

        let counters: HashMap<String, Counter> =
            HashMap::from_iter(snapshot.counters().into_iter().map(|v| (v.name.clone(), v)));
        let gauges: HashMap<String, Gauge> =
            HashMap::from_iter(snapshot.gauges().into_iter().map(|v| (v.name.clone(), v)));
        let histograms: HashMap<String, Histogram> = HashMap::from_iter(
            snapshot
                .histograms()
                .into_iter()
                .map(|v| (v.name.clone(), v)),
        );

        Self {
            ts,
            duration,
            counters,
            gauges,
            histograms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GroupSchema, GroupSnapshot, MetricDesc, SnapshotV2, SnapshotV3};
    use metriken_types::Window;
    use std::sync::Arc;
    use std::time::Duration;

    fn desc(name: &str, metric: &str) -> MetricDesc {
        MetricDesc {
            name: name.to_string(),
            metadata: [("metric".to_string(), metric.to_string())].into(),
        }
    }

    fn v3() -> SnapshotV3 {
        let schema = GroupSchema {
            counters: vec![desc("0", "cpu_cycles"), desc("1", "cpu_instructions")],
            gauges: vec![],
            histograms: vec![],
        };
        let schema_hash = schema.hash();
        SnapshotV3 {
            systemtime: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
            duration: Duration::from_millis(10),
            metadata: [("source".to_string(), "rezolus".to_string())].into(),
            groups: vec![GroupSnapshot {
                name: "cpu_usage/percpu".to_string(),
                schema_hash,
                schema: Some(Arc::new(schema)),
                window: Some(Window::new(999_000, 999_400)),
                counters: vec![Some(7), None],
                gauges: vec![],
                histograms: vec![],
            }],
        }
    }

    fn v2() -> SnapshotV2 {
        SnapshotV2 {
            systemtime: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
            duration: Duration::from_millis(10),
            metadata: HashMap::new(),
            counters: vec![Counter::new("0".to_string(), 7, HashMap::new())],
            gauges: vec![],
            histograms: vec![],
        }
    }

    #[test]
    fn v3_and_equivalent_v2_hash_identically_for_parquet() {
        // The compatibility contract for MsgpackToParquet: a V3 snapshot and
        // the V2 snapshot describing the same readings produce the same
        // HashedSnapshot, so legacy parquet output is unchanged by V3 input.
        let hv3: HashedSnapshot = Snapshot::V3(v3()).into();
        let mut v2 = v2();
        v2.counters = vec![Counter::new(
            "0".to_string(),
            7,
            [("metric".to_string(), "cpu_cycles".to_string())].into(),
        )
        .with_window(Some(Window::new(999_000, 999_400)))];
        let hv2: HashedSnapshot = Snapshot::V2(v2).into();
        assert_eq!(hv3.ts, hv2.ts);
        assert_eq!(hv3.duration, hv2.duration);
        assert_eq!(hv3.counters.len(), hv2.counters.len());
        let (a, b) = (&hv3.counters["0"], &hv2.counters["0"]);
        assert_eq!(
            (a.value, &a.metadata, a.window),
            (b.value, &b.metadata, b.window)
        );
    }
}
