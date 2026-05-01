//! Diagnostic test for the LAG-over-LIST UDF bug.
//! Probes (a) chunk-boundary effects (>2048 rows) and (b) actual h2_delta+h2_total
//! pipeline using metriken-query-sql's real UDFs.

use duckdb::core::{DataChunkHandle, LogicalTypeHandle, LogicalTypeId};
use duckdb::ffi::duckdb_vector_size;
use duckdb::vscalar::{ScalarFunctionSignature, VScalar};
use duckdb::vtab::arrow::WritableVector;
use duckdb::Connection;

/// Two-arg UDF that reads ONLY arg 0, ignoring arg 1.
/// Used to test: does merely calling `input.list_vector(1)` for a LAG'd
/// argument corrupt arg 0's child data?
pub struct DbgReadFirstOnly;
impl VScalar for DbgReadFirstOnly {
    type State = ();
    unsafe fn invoke(
        _: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let vec_size = duckdb_vector_size();
        let in1 = input.list_vector(0);
        let _in0 = input.list_vector(1); // touch arg1, but don't read its child
        let in1_n = in1.len();
        eprintln!("[read_first_only] duckdb_vector_size()={vec_size}");
        let in1_data = in1.child(in1_n).as_slice_with_len::<u64>(in1_n).to_vec();
        let zeros: Vec<usize> = in1_data
            .iter()
            .enumerate()
            .filter_map(|(i, &v)| if v == 0 && i != 0 { Some(i) } else { None })
            .take(5)
            .collect();
        eprintln!(
            "[read_first_only] n={n} in1_n={in1_n} in1[0..4]={:?} in1[2046..2050]={:?} \
             unexpected_zero_indices={:?}",
            &in1_data[0..4.min(in1_data.len())],
            &in1_data[2046.min(in1_n)..2050.min(in1_n)],
            zeros
        );

        let mut out = output.flat_vector();
        for r in 0..n {
            out.set_null(r);
        }
        Ok(())
    }

    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::list(&LogicalTypeId::UBigint.into()),
                LogicalTypeHandle::list(&LogicalTypeId::UBigint.into()),
            ],
            LogicalTypeId::UBigint.into(),
        )]
    }
}

/// h2_delta clone that does NOT call reserve on input children (uses child(0)
/// followed by as_slice_with_len(in_n)). If this produces correct results for
/// all rows, the workaround is confirmed.
pub struct DbgDeltaNoReserve;
impl VScalar for DbgDeltaNoReserve {
    type State = ();
    unsafe fn invoke(
        _: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let in1 = input.list_vector(0);
        let in0 = input.list_vector(1);
        let in1_n = in1.len();
        let in0_n = in0.len();
        // KEY DIFFERENCE: child(0) instead of child(in_n) — avoids the
        // reserve-zeroes-data bug.
        let in1_data = in1.child(0).as_slice_with_len::<u64>(in1_n).to_vec();
        let in0_data = in0.child(0).as_slice_with_len::<u64>(in0_n).to_vec();

        let mut total = 0usize;
        let mut plans: Vec<(usize, Option<(usize, usize, usize)>)> = Vec::with_capacity(n);
        for r in 0..n {
            let e1 = in1.get_entry(r);
            let e0 = in0.get_entry(r);
            let valid = e1.0.saturating_add(e1.1) <= in1_n
                && e0.0.saturating_add(e0.1) <= in0_n;
            let plan = valid.then(|| (e1.1.min(e0.1), e1.0, e0.0));
            let len = plan.as_ref().map_or(0, |(l, _, _)| *l);
            plans.push((total, plan));
            total += len;
        }

        let mut out_lv = output.list_vector();
        let mut out_child = out_lv.child(total);
        let out_data = out_child.as_mut_slice_with_len::<u64>(total);

        for (r, (off, plan)) in plans.into_iter().enumerate() {
            match plan {
                None => {
                    out_lv.set_entry(r, off, 0);
                    out_lv.set_null(r);
                }
                Some((len, off1, off0)) => {
                    for i in 0..len {
                        out_data[off + i] =
                            in1_data[off1 + i].saturating_sub(in0_data[off0 + i]);
                    }
                    out_lv.set_entry(r, off, len);
                }
            }
        }
        out_lv.set_len(total);
        Ok(())
    }

    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::list(&LogicalTypeId::UBigint.into()),
                LogicalTypeHandle::list(&LogicalTypeId::UBigint.into()),
            ],
            LogicalTypeHandle::list(&LogicalTypeId::UBigint.into()),
        )]
    }
}

/// Same as DbgIgnoreSecond but reads arg 0's child via *raw FFI* without calling
/// the higher-level `child(capacity)` (which triggers `duckdb_list_vector_reserve`).
/// If THIS still shows zeros at index 2048+, the corruption is in the input as
/// passed by DuckDB — i.e., before our UDF runs.
pub struct DbgRawRead;
impl VScalar for DbgRawRead {
    type State = ();
    unsafe fn invoke(
        _: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let in1 = input.list_vector(0);
        let in1_n = in1.len();
        // Reach through to the child via FFI without triggering reserve().
        // ListVector wraps a FlatVector; we call child(in1_n) just enough to
        // get the underlying ptr, but we test a min-capacity probe first.
        let in1_data_min = in1.child(0).as_slice_with_len::<u64>(in1_n).to_vec();
        eprintln!(
            "[raw_read] (child(0) probe) in1_n={in1_n} in1[2046..2050]={:?} \
             unexpected_zero_indices={:?}",
            &in1_data_min[2046..2050],
            in1_data_min
                .iter()
                .enumerate()
                .filter_map(|(i, &v)| (v == 0 && i != 0).then_some(i))
                .take(5)
                .collect::<Vec<_>>()
        );
        let in1_data_full = in1.child(in1_n).as_slice_with_len::<u64>(in1_n).to_vec();
        eprintln!(
            "[raw_read] (child(in1_n) probe) in1[2046..2050]={:?} \
             unexpected_zero_indices={:?}",
            &in1_data_full[2046..2050],
            in1_data_full
                .iter()
                .enumerate()
                .filter_map(|(i, &v)| (v == 0 && i != 0).then_some(i))
                .take(5)
                .collect::<Vec<_>>()
        );

        let mut out = output.flat_vector();
        for r in 0..n {
            out.set_null(r);
        }
        Ok(())
    }

    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::list(&LogicalTypeId::UBigint.into()),
                LogicalTypeHandle::list(&LogicalTypeId::UBigint.into()),
            ],
            LogicalTypeId::UBigint.into(),
        )]
    }
}

/// Two-arg UDF that reads only arg 0 and never even references arg 1.
pub struct DbgIgnoreSecond;
impl VScalar for DbgIgnoreSecond {
    type State = ();
    unsafe fn invoke(
        _: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let in1 = input.list_vector(0);
        let in1_n = in1.len();
        let in1_data = in1.child(in1_n).as_slice_with_len::<u64>(in1_n).to_vec();
        let zeros: Vec<usize> = in1_data
            .iter()
            .enumerate()
            .filter_map(|(i, &v)| if v == 0 && i != 0 { Some(i) } else { None })
            .take(5)
            .collect();
        eprintln!(
            "[ignore_second] n={n} in1_n={in1_n} in1[0..4]={:?} in1[2046..2050]={:?} \
             unexpected_zero_indices={:?}",
            &in1_data[0..4.min(in1_data.len())],
            &in1_data[2046.min(in1_n)..2050.min(in1_n)],
            zeros
        );

        let mut out = output.flat_vector();
        for r in 0..n {
            out.set_null(r);
        }
        Ok(())
    }

    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::list(&LogicalTypeId::UBigint.into()),
                LogicalTypeHandle::list(&LogicalTypeId::UBigint.into()),
            ],
            LogicalTypeId::UBigint.into(),
        )]
    }
}

/// Plain list-passthrough — no LAG involved. Tests whether the bug is in the
/// OUTPUT-write path independent of LAG.
pub struct DbgListPassthroughUdf;
impl VScalar for DbgListPassthroughUdf {
    type State = ();
    unsafe fn invoke(
        _: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let in_lv = input.list_vector(0);
        let in_n = in_lv.len();
        let in_data = in_lv.child(in_n).as_slice_with_len::<u64>(in_n).to_vec();

        let mut total = 0usize;
        let mut plans: Vec<(usize, Option<(usize, usize)>)> = Vec::with_capacity(n);
        for r in 0..n {
            let (off, len) = in_lv.get_entry(r);
            let valid = off.saturating_add(len) <= in_n;
            let plan = valid.then_some((len, off));
            let plen = plan.as_ref().map_or(0, |(l, _)| *l);
            plans.push((total, plan));
            total += plen;
        }

        let mut out_lv = output.list_vector();
        let mut out_child = out_lv.child(total);
        let out_data = out_child.as_mut_slice_with_len::<u64>(total);

        for (r, (off_out, plan)) in plans.into_iter().enumerate() {
            match plan {
                None => {
                    out_lv.set_entry(r, off_out, 0);
                    out_lv.set_null(r);
                }
                Some((len, off_in)) => {
                    for i in 0..len {
                        out_data[off_out + i] = in_data[off_in + i];
                    }
                    out_lv.set_entry(r, off_out, len);
                }
            }
        }
        out_lv.set_len(total);
        Ok(())
    }

    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![LogicalTypeHandle::list(&LogicalTypeId::UBigint.into())],
            LogicalTypeHandle::list(&LogicalTypeId::UBigint.into()),
        )]
    }
}

/// Variant that reserves 4x as much output capacity as needed.
/// If this fixes the bug, the issue is "duckdb_list_vector_reserve under-allocates".
pub struct DbgDeltaOversizeUdf;
impl VScalar for DbgDeltaOversizeUdf {
    type State = ();
    unsafe fn invoke(
        _: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let in1 = input.list_vector(0);
        let in0 = input.list_vector(1);
        let in1_n = in1.len();
        let in0_n = in0.len();
        let in1_data = in1.child(in1_n).as_slice_with_len::<u64>(in1_n).to_vec();
        let in0_data = in0.child(in0_n).as_slice_with_len::<u64>(in0_n).to_vec();

        let mut total = 0usize;
        let mut plans: Vec<(usize, Option<(usize, usize, usize)>)> = Vec::with_capacity(n);
        for r in 0..n {
            let e1 = in1.get_entry(r);
            let e0 = in0.get_entry(r);
            let valid = e1.0.saturating_add(e1.1) <= in1_n
                && e0.0.saturating_add(e0.1) <= in0_n;
            let plan = valid.then(|| (e1.1.min(e0.1), e1.0, e0.0));
            let len = plan.as_ref().map_or(0, |(l, _, _)| *l);
            plans.push((total, plan));
            total += len;
        }

        let mut out_lv = output.list_vector();
        // 4x oversize.
        let oversize = total.max(4) * 4;
        let mut out_child = out_lv.child(oversize);
        let out_data = out_child.as_mut_slice_with_len::<u64>(oversize);
        eprintln!(
            "[oversize] n={n} total={total} oversize={oversize} ptr={:p}",
            out_data.as_ptr()
        );

        for (r, (off, plan)) in plans.into_iter().enumerate() {
            match plan {
                None => {
                    out_lv.set_entry(r, off, 0);
                    out_lv.set_null(r);
                }
                Some((len, off1, off0)) => {
                    for i in 0..len {
                        out_data[off + i] =
                            in1_data[off1 + i].saturating_sub(in0_data[off0 + i]);
                    }
                    out_lv.set_entry(r, off, len);
                }
            }
        }
        out_lv.set_len(total);
        Ok(())
    }

    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::list(&LogicalTypeId::UBigint.into()),
                LogicalTypeHandle::list(&LogicalTypeId::UBigint.into()),
            ],
            LogicalTypeHandle::list(&LogicalTypeId::UBigint.into()),
        )]
    }
}

/// Variant that re-grabs the output child pointer for every single write.
/// If this avoids the corruption, the bug is "child pointer becomes stale
/// mid-loop"—which would point at duckdb-rs's lifetime / borrowing model.
pub struct DbgDeltaRefetchUdf;
impl VScalar for DbgDeltaRefetchUdf {
    type State = ();
    unsafe fn invoke(
        _: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let in1 = input.list_vector(0);
        let in0 = input.list_vector(1);
        let in1_n = in1.len();
        let in0_n = in0.len();
        let in1_data = in1.child(in1_n).as_slice_with_len::<u64>(in1_n).to_vec();
        let in0_data = in0.child(in0_n).as_slice_with_len::<u64>(in0_n).to_vec();

        let mut total = 0usize;
        let mut plans: Vec<(usize, Option<(usize, usize, usize)>)> = Vec::with_capacity(n);
        for r in 0..n {
            let e1 = in1.get_entry(r);
            let e0 = in0.get_entry(r);
            let valid = e1.0.saturating_add(e1.1) <= in1_n
                && e0.0.saturating_add(e0.1) <= in0_n;
            let plan = valid.then(|| (e1.1.min(e0.1), e1.0, e0.0));
            let len = plan.as_ref().map_or(0, |(l, _, _)| *l);
            plans.push((total, plan));
            total += len;
        }

        let mut out_lv = output.list_vector();
        // Reserve once, but re-fetch the data pointer for each write.
        out_lv.child(total);

        for (r, (off, plan)) in plans.into_iter().enumerate() {
            match plan {
                None => {
                    out_lv.set_entry(r, off, 0);
                    out_lv.set_null(r);
                }
                Some((len, off1, off0)) => {
                    // Re-grab pointer for this write.
                    let mut child = out_lv.child(total);
                    let slice = child.as_mut_slice_with_len::<u64>(total);
                    for i in 0..len {
                        slice[off + i] =
                            in1_data[off1 + i].saturating_sub(in0_data[off0 + i]);
                    }
                    out_lv.set_entry(r, off, len);
                }
            }
        }
        out_lv.set_len(total);
        Ok(())
    }

    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::list(&LogicalTypeId::UBigint.into()),
                LogicalTypeHandle::list(&LogicalTypeId::UBigint.into()),
            ],
            LogicalTypeHandle::list(&LogicalTypeId::UBigint.into()),
        )]
    }
}

/// Mimics h2_delta but prints diagnostics about input/output capacities and the
/// state at each row. Bug surfaces around row 1024, so we focus prints there.
pub struct DbgDeltaUdf;
impl VScalar for DbgDeltaUdf {
    type State = ();
    unsafe fn invoke(
        _: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let in1 = input.list_vector(0);
        let in0 = input.list_vector(1);
        let in1_n = in1.len();
        let in0_n = in0.len();
        let in1_data = in1.child(in1_n).as_slice_with_len::<u64>(in1_n).to_vec();
        let in0_data = in0.child(in0_n).as_slice_with_len::<u64>(in0_n).to_vec();
        eprintln!(
            "[dbg_delta] n={n} in1_n={in1_n} in0_n={in0_n}",
        );
        // Print samples around the suspect boundary.
        let idxs = [0usize, 1, 2044, 2045, 2046, 2047, 2048, 2049, 2050, 2051, 4090, 4091, 4092, 4093];
        for &i in &idxs {
            if i < in0_n {
                eprintln!(
                    "    in1_data[{i}]={:?} in0_data[{i}]={:?}",
                    in1_data.get(i),
                    in0_data.get(i)
                );
            }
        }
        // Anomaly scan: any in0_data[i] != i?
        let mut anomalies: Vec<(usize, u64)> = Vec::new();
        for i in 0..in0_n {
            if in0_data[i] != i as u64 {
                anomalies.push((i, in0_data[i]));
                if anomalies.len() >= 5 {
                    break;
                }
            }
        }
        eprintln!("    in0_data anomalies (first 5): {:?}", anomalies);

        let mut total = 0usize;
        let mut plans: Vec<(usize, Option<(usize, usize, usize)>)> = Vec::with_capacity(n);
        for r in 0..n {
            let e1 = in1.get_entry(r);
            let e0 = in0.get_entry(r);
            let valid_1 = e1.0.saturating_add(e1.1) <= in1_n;
            let valid_0 = e0.0.saturating_add(e0.1) <= in0_n;
            let plan = if valid_1 && valid_0 {
                let len = e1.1.min(e0.1);
                Some((len, e1.0, e0.0))
            } else {
                None
            };
            let len = plan.as_ref().map_or(0, |(l, _, _)| *l);
            plans.push((total, plan));
            total += len;
        }
        eprintln!("[dbg_delta] total_child={total} (will reserve)");

        let mut out_lv = output.list_vector();
        let mut out_child = out_lv.child(total);
        let out_ptr_before = out_child.as_mut_ptr::<u64>() as usize;
        let out_data = out_child.as_mut_slice_with_len::<u64>(total);
        eprintln!(
            "[dbg_delta] out_child ptr=0x{:x} slice_len={}",
            out_ptr_before,
            out_data.len()
        );

        for (r, (off, plan)) in plans.into_iter().enumerate() {
            // Show what we're about to do at the boundary.
            if r == 1023 || r == 1024 || r == 1025 || r == n - 1 {
                eprintln!("[dbg_delta]   r={r} off={off} plan={plan:?}");
            }
            match plan {
                None => {
                    out_lv.set_entry(r, off, 0);
                    out_lv.set_null(r);
                }
                Some((len, off1, off0)) => {
                    for i in 0..len {
                        out_data[off + i] = in1_data[off1 + i].saturating_sub(in0_data[off0 + i]);
                    }
                    out_lv.set_entry(r, off, len);
                }
            }
        }
        out_lv.set_len(total);

        // Now read the output back via raw pointer to confirm what got written.
        let after_ptr = out_lv.child(total).as_mut_ptr::<u64>() as usize;
        let after_slice = out_lv.child(total).as_slice_with_len::<u64>(total).to_vec();
        eprintln!(
            "[dbg_delta] AFTER WRITE: child ptr=0x{:x} (was 0x{:x}, same={}), \
             out_data[2046..2050]={:?}",
            after_ptr,
            out_ptr_before,
            after_ptr == out_ptr_before,
            after_slice
                .get(2046..2050.min(after_slice.len()))
                .map(|s| s.to_vec())
        );
        Ok(())
    }

    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::list(&LogicalTypeId::UBigint.into()),
                LogicalTypeHandle::list(&LogicalTypeId::UBigint.into()),
            ],
            LogicalTypeHandle::list(&LogicalTypeId::UBigint.into()),
        )]
    }
}

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
    conn.register_scalar_function::<DbgDeltaUdf>("dbg_delta").unwrap();
    conn.register_scalar_function::<DbgDeltaRefetchUdf>("dbg_delta_refetch").unwrap();
    conn.register_scalar_function::<DbgDeltaOversizeUdf>("dbg_delta_oversize").unwrap();
    conn.register_scalar_function::<DbgListPassthroughUdf>("dbg_passthrough").unwrap();
    conn.register_scalar_function::<DbgReadFirstOnly>("dbg_read_first_only").unwrap();
    conn.register_scalar_function::<DbgIgnoreSecond>("dbg_ignore_second").unwrap();
    conn.register_scalar_function::<DbgRawRead>("dbg_raw_read").unwrap();
    conn.register_scalar_function::<DbgDeltaNoReserve>("dbg_delta_noreserve").unwrap();
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
fn fixed_delta_works() {
    for &n in &[100usize, 2048, 3000, 5000] {
        let conn = make_conn(n);
        let mut s = conn
            .prepare(
                "SELECT ts, dbg_delta_noreserve(buckets, LAG(buckets) OVER (ORDER BY ts)) AS d \
                 FROM t ORDER BY ts",
            )
            .unwrap();
        let rows: Vec<(u64, Option<Vec<u64>>)> = s
            .query_map([], |row| {
                let ts: u64 = row.get(0)?;
                let v: Option<duckdb::types::Value> = row.get(1)?;
                let lst = v.map(|val| match val {
                    duckdb::types::Value::List(items) => items
                        .into_iter()
                        .map(|x| match x {
                            duckdb::types::Value::UBigInt(u) => u,
                            _ => 0,
                        })
                        .collect(),
                    _ => Vec::new(),
                });
                Ok((ts, lst))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        let mut wrong = 0usize;
        for (i, (_ts, lst)) in rows.iter().enumerate() {
            // Row 0: LAG is NULL — accept None / Some([]).
            // Other rows: must be exactly [2, 2].
            let ok = if i == 0 {
                matches!(lst, None) || matches!(lst, Some(v) if v.is_empty())
            } else {
                lst.as_deref() == Some(&[2u64, 2u64][..])
            };
            if !ok {
                wrong += 1;
                if wrong <= 3 {
                    eprintln!("    n={n} row={i} got={:?}", lst);
                }
            }
        }
        eprintln!("[fixed_delta] n={n}: total={} wrong={}", rows.len(), wrong);
        assert_eq!(wrong, 0, "fixed h2_delta should produce no wrong rows for n={n}");
    }
}

#[test]
fn raw_read_does_reserve_zero_data() {
    let conn = make_conn(2048);
    eprintln!("\n-- with LAG arg --");
    conn.prepare(
        "SELECT dbg_raw_read(buckets, LAG(buckets) OVER (ORDER BY ts)) FROM t ORDER BY ts",
    )
    .unwrap()
    .query_map([], |_| Ok(()))
    .unwrap()
    .for_each(drop);
    eprintln!("\n-- with non-LAG arg (sanity) --");
    conn.prepare("SELECT dbg_raw_read(buckets, buckets) FROM t ORDER BY ts")
        .unwrap()
        .query_map([], |_| Ok(()))
        .unwrap()
        .for_each(drop);
}

#[test]
fn lag_workarounds() {
    let conn = make_conn(2048);
    let cases: &[(&str, &str)] = &[
        ("D1: list_value(1::UBIGINT) constant", "[1::UBIGINT]"),
        ("D2: same column twice (no LAG)", "buckets"),
        ("D3: LAG over scalar then list it", "[LAG(ts) OVER (ORDER BY ts)]"),
        ("D4: LAG(buckets) cast roundtrip", "CAST(LAG(buckets) OVER (ORDER BY ts) AS UBIGINT[])"),
        ("D5: list_concat(LAG(...), [])", "list_concat(LAG(buckets) OVER (ORDER BY ts), CAST([] AS UBIGINT[]))"),
        ("D6: vanilla LAG(buckets)", "LAG(buckets) OVER (ORDER BY ts)"),
    ];
    for (label, expr) in cases {
        let q = format!("SELECT dbg_ignore_second(buckets, {expr}) FROM t ORDER BY ts");
        eprintln!("\n-- {label}: {expr} --");
        match conn.prepare(&q) {
            Ok(mut s) => match s.query_map([], |_| Ok(())) {
                Ok(it) => it.for_each(|r| { let _ = r; }),
                Err(e) => eprintln!("    EXEC ERR: {e}"),
            },
            Err(e) => eprintln!("    PREPARE ERR: {e}"),
        }
    }
}

#[test]
fn isolate_lag_corruption() {
    let conn = make_conn(2048);
    eprintln!("\n-- A: dbg_ignore_second(buckets, LAG(buckets) ...) — never even fetches arg 1 --");
    conn.prepare(
        "SELECT dbg_ignore_second(buckets, LAG(buckets) OVER (ORDER BY ts)) FROM t ORDER BY ts",
    )
    .unwrap()
    .query_map([], |_| Ok(()))
    .unwrap()
    .for_each(drop);

    eprintln!("\n-- B: dbg_read_first_only(buckets, LAG(buckets) ...) — fetches arg1 list_vector but doesn't touch its child --");
    conn.prepare(
        "SELECT dbg_read_first_only(buckets, LAG(buckets) OVER (ORDER BY ts)) FROM t ORDER BY ts",
    )
    .unwrap()
    .query_map([], |_| Ok(()))
    .unwrap()
    .for_each(drop);

    eprintln!("\n-- C: dbg_ignore_second(buckets, buckets) — no LAG at all --");
    conn.prepare("SELECT dbg_ignore_second(buckets, buckets) FROM t ORDER BY ts")
        .unwrap()
        .query_map([], |_| Ok(()))
        .unwrap()
        .for_each(drop);
}

#[test]
fn passthrough_no_lag() {
    // No LAG — just identity. If this corrupts at row 1024, the bug is purely
    // in write_list_output's output path, NOT LAG.
    for &n in &[2048usize, 3000] {
        let conn = make_conn(n);
        let mut s = conn
            .prepare("SELECT ts, dbg_passthrough(buckets) AS d FROM t ORDER BY ts")
            .unwrap();
        let rows: Vec<(u64, Vec<u64>)> = s
            .query_map([], |row| {
                let ts: u64 = row.get(0)?;
                let v: duckdb::types::Value = row.get(1)?;
                let lst = match v {
                    duckdb::types::Value::List(items) => items
                        .into_iter()
                        .map(|x| match x {
                            duckdb::types::Value::UBigInt(u) => u,
                            _ => 0,
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                Ok((ts, lst))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        let mut wrong = 0usize;
        for (i, (_ts, lst)) in rows.iter().enumerate() {
            let expected = vec![(i * 2) as u64, (i * 2 + 1) as u64];
            if *lst != expected {
                wrong += 1;
                if wrong <= 3 {
                    eprintln!("    row={i} got={:?} want={:?}", lst, expected);
                }
            }
        }
        eprintln!("[passthrough] n={n}: total={} wrong={}", rows.len(), wrong);
    }
}

#[test]
fn oversize_variant_correctness() {
    for &n in &[2048usize, 3000, 5000] {
        let conn = make_conn(n);
        let mut s = conn
            .prepare(
                "SELECT ts, dbg_delta_oversize(buckets, LAG(buckets) OVER (ORDER BY ts)) AS d \
                 FROM t ORDER BY ts",
            )
            .unwrap();
        let rows: Vec<(u64, Option<Vec<u64>>)> = s
            .query_map([], |row| {
                let ts: u64 = row.get(0)?;
                let v: Option<duckdb::types::Value> = row.get(1)?;
                let lst = v.map(|val| match val {
                    duckdb::types::Value::List(items) => items
                        .into_iter()
                        .map(|x| match x {
                            duckdb::types::Value::UBigInt(u) => u,
                            _ => 0,
                        })
                        .collect(),
                    _ => Vec::new(),
                });
                Ok((ts, lst))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        let mut wrong = 0usize;
        for (i, (_ts, lst)) in rows.iter().enumerate() {
            let expected = if i == 0 { None } else { Some(vec![2u64, 2u64]) };
            if *lst != expected {
                wrong += 1;
            }
        }
        eprintln!("[oversize] n={n}: total={} wrong={}", rows.len(), wrong);
    }
}

#[test]
fn refetch_variant_correctness() {
    // Same pipeline as the buggy one, but using DbgDeltaRefetchUdf.
    for &n in &[2048usize, 3000, 5000] {
        let conn = make_conn(n);
        let mut s = conn
            .prepare(
                "SELECT ts, dbg_delta_refetch(buckets, LAG(buckets) OVER (ORDER BY ts)) AS d \
                 FROM t ORDER BY ts",
            )
            .unwrap();
        let rows: Vec<(u64, Option<Vec<u64>>)> = s
            .query_map([], |row| {
                let ts: u64 = row.get(0)?;
                let v: Option<duckdb::types::Value> = row.get(1)?;
                let lst = v.map(|val| match val {
                    duckdb::types::Value::List(items) => items
                        .into_iter()
                        .map(|x| match x {
                            duckdb::types::Value::UBigInt(u) => u,
                            _ => 0,
                        })
                        .collect(),
                    _ => Vec::new(),
                });
                Ok((ts, lst))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        let mut wrong = 0usize;
        for (i, (_ts, lst)) in rows.iter().enumerate() {
            let expected = if i == 0 { None } else { Some(vec![2u64, 2u64]) };
            if *lst != expected {
                wrong += 1;
            }
        }
        eprintln!("[refetch] n={n}: total={} wrong={}", rows.len(), wrong);
    }
}

#[test]
fn dbg_delta_at_boundary() {
    let conn = make_conn(2048);
    eprintln!("\n-- dbg_delta(buckets, LAG(buckets) OVER ...) n=2048 --");
    let mut s = conn
        .prepare(
            "SELECT ts, dbg_delta(buckets, LAG(buckets) OVER (ORDER BY ts)) AS d \
             FROM t ORDER BY ts",
        )
        .unwrap();
    let rows: Vec<(u64, Option<Vec<u64>>)> = s
        .query_map([], |row| {
            let ts: u64 = row.get(0)?;
            let v: Option<duckdb::types::Value> = row.get(1)?;
            let lst = v.map(|val| match val {
                duckdb::types::Value::List(items) => items
                    .into_iter()
                    .map(|x| match x {
                        duckdb::types::Value::UBigInt(u) => u,
                        _ => 0,
                    })
                    .collect(),
                _ => Vec::new(),
            });
            Ok((ts, lst))
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    let mut wrong = Vec::new();
    for (i, (_ts, lst)) in rows.iter().enumerate() {
        let expected = if i == 0 { None } else { Some(vec![2u64, 2u64]) };
        if *lst != expected {
            wrong.push((i, lst.clone()));
        }
    }
    eprintln!("dbg_delta wrong rows: {}", wrong.len());
    for w in wrong.iter().take(10) {
        eprintln!("    row={} got={:?}", w.0, w.1);
    }
}

#[test]
fn h2_delta_output_only() {
    // Inspect h2_delta's OUTPUT directly (no h2_total wrapping it).
    // If the values [a, b] = [2, 2] always, the bug is downstream of h2_delta.
    // If they're wrong, the bug is in h2_delta's read or write.
    for &n in &[2048usize, 3000] {
        let conn = make_conn(n);
        let mut s = conn
            .prepare(
                "SELECT ts, h2_delta(buckets, LAG(buckets) OVER (ORDER BY ts)) AS dlist \
                 FROM t ORDER BY ts",
            )
            .unwrap();
        let rows: Vec<(u64, Option<Vec<u64>>)> = s
            .query_map([], |row| {
                let ts: u64 = row.get(0)?;
                let v: Option<duckdb::types::Value> = row.get(1)?;
                let lst = v.map(|val| match val {
                    duckdb::types::Value::List(items) => items
                        .into_iter()
                        .map(|x| match x {
                            duckdb::types::Value::UBigInt(u) => u,
                            _ => 0,
                        })
                        .collect(),
                    _ => Vec::new(),
                });
                Ok((ts, lst))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        let mut wrong = Vec::new();
        for (i, (ts, lst)) in rows.iter().enumerate() {
            let expected = if i == 0 { None } else { Some(vec![2u64, 2u64]) };
            if *lst != expected {
                wrong.push((i, *ts, lst.clone(), expected));
            }
        }
        eprintln!("[h2_delta only] n={n}: total={} wrong={}", rows.len(), wrong.len());
        for w in wrong.iter().take(5) {
            eprintln!("    row={} ts={} got={:?} want={:?}", w.0, w.1, w.2, w.3);
        }
    }
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
