//! Diagnostic test for the LAG-over-LIST UDF bug.
//! Probes (a) chunk-boundary effects (>2048 rows) and (b) actual h2_delta+h2_total
//! pipeline using metriken-query-sql's real UDFs.

use duckdb::core::{DataChunkHandle, LogicalTypeHandle, LogicalTypeId};
use duckdb::vscalar::{ScalarFunctionSignature, VScalar};
use duckdb::vtab::arrow::WritableVector;
use duckdb::Connection;

pub struct DbgListUdf;
impl VScalar for DbgListUdf {
    type State = ();
    unsafe fn invoke(
        _: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let lv = input.list_vector(0);
        let child_n = lv.len();
        let data = lv.child(child_n).as_slice_with_len::<u64>(child_n).to_vec();
        let mut max_end = 0usize;
        let mut bad = 0usize;
        for r in 0..n {
            let (off, len) = lv.get_entry(r);
            let end = off.saturating_add(len);
            if end > child_n {
                bad += 1;
            }
            max_end = max_end.max(end);
        }
        eprintln!(
            "[dbg_list] n={n} child_n={child_n} max_end={max_end} bad_entries={bad} \
             first8_of_child={:?} last8_of_child={:?}",
            &data.iter().take(8).collect::<Vec<_>>(),
            &data.iter().rev().take(8).rev().collect::<Vec<_>>()
        );

        let mut out = output.flat_vector();
        for r in 0..n {
            out.set_null(r);
        }
        Ok(())
    }

    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![LogicalTypeHandle::list(&LogicalTypeId::UBigint.into())],
            LogicalTypeId::UBigint.into(),
        )]
    }
}

fn make_conn(rows: usize) -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.register_scalar_function::<DbgListUdf>("dbg_list").unwrap();
    metriken_query_sql::udf::register_all(&conn).unwrap();
    conn.execute_batch(&format!(
        r#"
        CREATE TABLE t AS
        SELECT (i*1000000000)::UBIGINT AS ts,
               [(i*2)::UBIGINT, (i*2+1)::UBIGINT] AS buckets
        FROM range({rows}) t(i);
        "#
    ))
    .unwrap();
    conn
}

#[test]
fn small_dataset_dbg() {
    let conn = make_conn(11);
    eprintln!("\n-- small (11): dbg_list direct --");
    conn.prepare("SELECT dbg_list(buckets) FROM t ORDER BY ts")
        .unwrap()
        .query_map([], |_| Ok(()))
        .unwrap()
        .for_each(drop);
    eprintln!("\n-- small (11): dbg_list of LAG --");
    conn.prepare("SELECT dbg_list(LAG(buckets) OVER (ORDER BY ts)) FROM t ORDER BY ts")
        .unwrap()
        .query_map([], |_| Ok(()))
        .unwrap()
        .for_each(drop);
}

#[test]
fn large_dataset_dbg() {
    // > 2048 rows triggers chunked execution.
    let conn = make_conn(3000);
    eprintln!("\n-- large (3000): dbg_list of LAG --");
    conn.prepare("SELECT dbg_list(LAG(buckets) OVER (ORDER BY ts)) FROM t ORDER BY ts")
        .unwrap()
        .query_map([], |_| Ok(()))
        .unwrap()
        .for_each(drop);
}

#[test]
fn h2_pipeline_correctness() {
    // The actual reported bug: h2_total(h2_delta(buckets, LAG(buckets) OVER ...))
    for &n in &[11usize, 100, 2048, 2049, 3000, 5000] {
        let conn = make_conn(n);
        // Each row's buckets[i] = 2*ts_idx + i, so cumulative-style.
        // Delta vs LAG: row[r] - row[r-1] = [2, 2] for each bucket.
        // h2_total of [2,2] = 4 for every non-first row.
        let mut s = conn
            .prepare(
                "SELECT ts, h2_total(h2_delta(buckets, LAG(buckets) OVER (ORDER BY ts))) AS dt \
                 FROM t ORDER BY ts",
            )
            .unwrap();
        let rows: Vec<(u64, Option<u64>)> = s
            .query_map([], |row| {
                Ok((row.get::<_, u64>(0)?, row.get::<_, Option<u64>>(1)?))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        let mut wrong = Vec::new();
        for (i, (ts, dt)) in rows.iter().enumerate() {
            let expected = if i == 0 { None } else { Some(4u64) };
            if *dt != expected {
                wrong.push((i, *ts, *dt, expected));
            }
        }
        eprintln!(
            "n={n}: total={} wrong={} first_wrong={:?}",
            rows.len(),
            wrong.len(),
            wrong.first()
        );
        if !wrong.is_empty() {
            for w in wrong.iter().take(8) {
                eprintln!("    {:?}", w);
            }
        }
    }
}
