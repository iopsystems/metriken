// H2 histogram UDFs.
//
// All UDFs accept `p` (grouping_power) as their last argument. `n` is fixed at
// 64 (the only size used in this dataset). For columns where p=3 is implied
// (rezolus metrics.parquet), there are 1-arg-shorter overloads that default to
// p=3. llmperf.parquet uses p=7, so always pass it explicitly there.
//
// **Known issue:** `h2_delta(buckets, LAG(buckets) OVER (ORDER BY ts))` reads
// wrong child-vector contents at chunk boundaries — the LAG'd list_vector's
// child layout doesn't match what `in0.child(in0.len())` returns through the
// duckdb-rs `vscalar` API, so deltas are corrupted at the last row of large
// chunks. As a workaround, catalogue queries should use a self-join
// (`LEFT JOIN ... ON b.ts = a.ts - 1Hz_interval`) instead of LAG when computing
// histogram deltas. The bug only surfaces with LIST<UBIGINT> + LAG; scalar
// LAG (used in `irate_1s` / `rate_5m`) is unaffected.
//
// Bucket bounds are **inclusive on both ends**, matching the canonical
// `histogram` crate (used by rezolus itself — see its `Bucket::start/end`).
// `h2_upper(idx)` is the largest value the bucket can hold; the top bucket's
// upper is exactly `u64::MAX`. Quantiles return the inclusive upper of the
// bucket containing the q-th sample (nearest-rank); empty histogram → NULL.
//
// duckdb-rs gotcha: `ListVector::get_entry` reads from raw `duckdb_list_entry`
// memory and returns *uninitialized* (off, len) for NULL parent rows. There's
// no `row_is_null` exposed on `ListVector`, so every UDF that touches a list
// input bounds-checks the entry against the child capacity (see `list_entry`
// below); out-of-range entries indicate NULL parents or malformed lists.

use duckdb::core::{DataChunkHandle, ListVector, LogicalTypeHandle, LogicalTypeId};
use duckdb::vscalar::{ScalarFunctionSignature, VScalar};
use duckdb::vtab::arrow::WritableVector;
use duckdb::Connection;

const N: u32 = 64;
const DEFAULT_P: u32 = 3;
const MAX_BUCKET_COUNT: usize = ((N - 2 + 1) * (1u32 << 14)) as usize; // generous upper

#[inline]
fn bucket_count(p: u32) -> u32 {
    (N - p + 1) * (1 << p)
}

#[inline]
fn valid_p(p: u32) -> bool {
    (2..=14).contains(&p)
}

#[inline]
fn h2_lower_p(idx: u32, p: u32) -> u64 {
    if idx < (1u32 << (p + 1)) {
        idx as u64
    } else {
        let w = (idx >> p) - 1;
        (1u64 << (p + w)) + ((idx & ((1 << p) - 1)) as u64) * (1u64 << w)
    }
}

#[inline]
fn h2_upper_p(idx: u32, p: u32) -> u64 {
    if idx + 1 == bucket_count(p) {
        // Top bucket — the inclusive upper of the value space.
        // (Computing it from the formula would overflow `2^p+w + h·2^w` past u64.)
        u64::MAX
    } else if idx < (1u32 << (p + 1)) {
        // Linear region: bucket [idx, idx] holds exactly the value `idx`.
        idx as u64
    } else {
        let w = (idx >> p) - 1;
        (1u64 << (p + w)) + (((idx & ((1 << p) - 1)) + 1) as u64) * (1u64 << w) - 1
    }
}

#[inline]
fn h2_mid_p(idx: u32, p: u32) -> u64 {
    let lo = h2_lower_p(idx, p);
    let hi = h2_upper_p(idx, p);
    lo + (hi - lo) / 2
}

fn ubig() -> LogicalTypeHandle { LogicalTypeId::UBigint.into() }
fn dbl()  -> LogicalTypeHandle { LogicalTypeId::Double.into() }
fn int()  -> LogicalTypeHandle { LogicalTypeId::Integer.into() }
fn ubig_list() -> LogicalTypeHandle { LogicalTypeHandle::list(&ubig()) }
fn dbl_list()  -> LogicalTypeHandle { LogicalTypeHandle::list(&dbl()) }

/// Bounds-check a list_vector row. Returns `None` for NULL parent rows
/// (whose `get_entry` is uninitialized) and malformed lists.
#[inline]
fn list_entry(lv: &ListVector<'_>, r: usize, child_n: usize) -> Option<(usize, usize)> {
    let (off, len) = lv.get_entry(r);
    (off.saturating_add(len) <= child_n).then_some((off, len))
}

/// Read an optional `INTEGER` p column at `idx`, defaulting to `DEFAULT_P` if absent.
fn read_p(input: &DataChunkHandle, idx: usize, n: usize) -> Vec<u32> {
    if input.num_columns() > idx {
        input.flat_vector(idx).as_slice_with_len::<i32>(n).iter().map(|&x| x as u32).collect()
    } else {
        vec![DEFAULT_P; n]
    }
}

/// Two-pass write of a `LIST<UBIGINT>` output column. Used by `h2_delta` and
/// `h2_quantiles`, which both produce list rows whose width depends on per-row
/// inputs and which both emit NULL for invalid rows.
///
/// The `plan` closure runs once per row and returns either `Some((len, state))`
/// for a valid row (the helper records `len` for layout and hands `state` to
/// the writer in pass 2) or `None` for a NULL row. Pass 2 calls `write` with
/// the planner's `state` and a mutable slice into the child buffer that is
/// exactly `len` long.
fn write_list_output<S, P, W>(
    n: usize,
    output: &mut dyn WritableVector,
    mut plan: P,
    mut write: W,
) where
    P: FnMut(usize) -> Option<(usize, S)>,
    W: FnMut(S, &mut [u64]),
{
    let mut entries: Vec<(usize, Option<(usize, S)>)> = Vec::with_capacity(n);
    let mut total = 0usize;
    for r in 0..n {
        let row = plan(r);
        let len = row.as_ref().map_or(0, |(l, _)| *l);
        entries.push((total, row));
        total += len;
    }

    let mut out_lv = output.list_vector();
    let mut out_child = out_lv.child(total);
    let out_data = out_child.as_mut_slice_with_len::<u64>(total);

    for (r, (off, row)) in entries.into_iter().enumerate() {
        match row {
            None => {
                out_lv.set_entry(r, off, 0);
                out_lv.set_null(r);
            }
            Some((len, state)) => {
                write(state, &mut out_data[off..off + len]);
                out_lv.set_entry(r, off, len);
            }
        }
    }
    out_lv.set_len(total);
}

// ----- bucket-bound scalars (idx, [p]) -> UBIGINT -----

macro_rules! bound_udf {
    ($T:ident, $fn:expr) => {
        pub struct $T;
        impl VScalar for $T {
            type State = ();
            unsafe fn invoke(
                _: &Self::State,
                input: &mut DataChunkHandle,
                output: &mut dyn WritableVector,
            ) -> Result<(), Box<dyn std::error::Error>> {
                let n = input.len();
                let in_idx = input.flat_vector(0);
                let xs = in_idx.as_slice_with_len::<i32>(n).to_vec();
                let ps = read_p(input, 1, n);
                let mut out_vec = output.flat_vector();
                let out_ptr = out_vec.as_mut_ptr::<u64>();
                for r in 0..n {
                    let raw = xs[r];
                    let p = ps[r];
                    if !valid_p(p)
                        || in_idx.row_is_null(r as u64)
                        || raw < 0
                        || (raw as u32) >= bucket_count(p)
                    {
                        out_vec.set_null(r);
                    } else {
                        unsafe { *out_ptr.add(r) = $fn(raw as u32, p); }
                    }
                }
                Ok(())
            }
            fn signatures() -> Vec<ScalarFunctionSignature> {
                vec![
                    ScalarFunctionSignature::exact(vec![int()], ubig()),
                    ScalarFunctionSignature::exact(vec![int(), int()], ubig()),
                ]
            }
        }
    };
}

bound_udf!(H2LowerUdf, h2_lower_p);
bound_udf!(H2UpperUdf, h2_upper_p);
bound_udf!(H2MidUdf,   h2_mid_p);

// ----- h2_total(buckets) -> UBIGINT (no p needed) -----

pub struct H2TotalUdf;
impl VScalar for H2TotalUdf {
    type State = ();
    unsafe fn invoke(
        _: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let in_lv = input.list_vector(0);
        let child_n = in_lv.len();
        let data = in_lv.child(child_n).as_slice_with_len::<u64>(child_n).to_vec();
        let mut out_vec = output.flat_vector();
        let out_ptr = out_vec.as_mut_ptr::<u64>();
        for r in 0..n {
            let Some((off, len)) = list_entry(&in_lv, r, child_n) else {
                out_vec.set_null(r);
                continue;
            };
            let mut total: u64 = 0;
            for i in 0..len {
                total = total.wrapping_add(data[off + i]);
            }
            unsafe { *out_ptr.add(r) = total; }
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(vec![ubig_list()], ubig())]
    }
}

// ----- h2_delta(b1, b0) -> UBIGINT[] (no p needed) -----

pub struct H2DeltaUdf;
impl VScalar for H2DeltaUdf {
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

        write_list_output(
            n,
            output,
            |r| {
                list_entry(&in1, r, in1_n)
                    .zip(list_entry(&in0, r, in0_n))
                    .map(|((off1, l1), (off0, l0))| {
                        let len = l1.min(l0);
                        (len, (off1, off0))
                    })
            },
            |(off1, off0), dst| {
                for (i, slot) in dst.iter_mut().enumerate() {
                    *slot = in1_data[off1 + i].saturating_sub(in0_data[off0 + i]);
                }
            },
        );
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(vec![ubig_list(), ubig_list()], ubig_list())]
    }
}

// ----- h2_quantile  -----
// Overloads:
//   h2_quantile(buckets, q)                           -- p=3 implicit
//   h2_quantile(buckets, q, p)                        -- explicit p
//   h2_quantile(buckets, q, lo, hi)                   -- p=3 implicit, range
//   h2_quantile(buckets, q, lo, hi, p)                -- explicit p, range

pub struct H2QuantileUdf;
impl VScalar for H2QuantileUdf {
    type State = ();
    unsafe fn invoke(
        _: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let ncols = input.num_columns();
        let in_lv = input.list_vector(0);
        let child_n = in_lv.len();
        let data = in_lv.child(child_n).as_slice_with_len::<u64>(child_n).to_vec();
        let qs = input.flat_vector(1).as_slice_with_len::<f64>(n).to_vec();

        // 4-/5-arg overloads carry (lo, hi); fold the no-range case into
        // (0, u64::MAX) so the bucket filter below is a uniform no-op there.
        let has_range = ncols >= 4;
        let (los, his) = if has_range {
            (
                input.flat_vector(2).as_slice_with_len::<u64>(n).to_vec(),
                input.flat_vector(3).as_slice_with_len::<u64>(n).to_vec(),
            )
        } else {
            (vec![0u64; n], vec![u64::MAX; n])
        };
        let ps = read_p(input, if has_range { 4 } else { 2 }, n);

        let mut out_vec = output.flat_vector();
        let out_ptr = out_vec.as_mut_ptr::<u64>();
        for r in 0..n {
            let p = ps[r];
            let Some((off, len)) = list_entry(&in_lv, r, child_n) else {
                out_vec.set_null(r);
                continue;
            };
            if !valid_p(p) {
                out_vec.set_null(r);
                continue;
            }
            let (lo, hi) = (los[r], his[r]);

            let in_range = |i: usize| {
                let blo = h2_lower_p(i as u32, p);
                let bhi = h2_upper_p(i as u32, p);
                blo >= lo && bhi <= hi
            };

            let mut total: u64 = 0;
            for i in 0..len {
                if in_range(i) {
                    total = total.saturating_add(data[off + i]);
                }
            }
            if total == 0 {
                out_vec.set_null(r);
                continue;
            }
            let q = qs[r].clamp(0.0, 1.0);
            let target: u64 = ((q * total as f64).ceil() as u64).max(1);
            let mut cumul: u64 = 0;
            let mut chosen: usize = len.saturating_sub(1);
            for i in 0..len {
                if !in_range(i) { continue; }
                cumul = cumul.saturating_add(data[off + i]);
                if cumul >= target {
                    chosen = i;
                    break;
                }
            }
            unsafe { *out_ptr.add(r) = h2_upper_p(chosen as u32, p); }
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![
            ScalarFunctionSignature::exact(vec![ubig_list(), dbl()], ubig()),
            ScalarFunctionSignature::exact(vec![ubig_list(), dbl(), int()], ubig()),
            ScalarFunctionSignature::exact(vec![ubig_list(), dbl(), ubig(), ubig()], ubig()),
            ScalarFunctionSignature::exact(vec![ubig_list(), dbl(), ubig(), ubig(), int()], ubig()),
        ]
    }
}

// ----- h2_count_in_range(buckets, lo, hi, [p]) -> UBIGINT -----

pub struct H2CountInRangeUdf;
impl VScalar for H2CountInRangeUdf {
    type State = ();
    unsafe fn invoke(
        _: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let in_lv = input.list_vector(0);
        let child_n = in_lv.len();
        let data = in_lv.child(child_n).as_slice_with_len::<u64>(child_n).to_vec();
        let los = input.flat_vector(1).as_slice_with_len::<u64>(n).to_vec();
        let his = input.flat_vector(2).as_slice_with_len::<u64>(n).to_vec();
        let ps = read_p(input, 3, n);
        let mut out_vec = output.flat_vector();
        let out_ptr = out_vec.as_mut_ptr::<u64>();
        for r in 0..n {
            let Some((off, len)) = list_entry(&in_lv, r, child_n) else {
                out_vec.set_null(r);
                continue;
            };
            let p = ps[r];
            let (lo, hi) = (los[r], his[r]);
            let mut total: u64 = 0;
            if valid_p(p) {
                for i in 0..len {
                    let blo = h2_lower_p(i as u32, p);
                    let bhi = h2_upper_p(i as u32, p);
                    if blo >= lo && bhi <= hi {
                        total = total.saturating_add(data[off + i]);
                    }
                }
            }
            unsafe { *out_ptr.add(r) = total; }
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![
            ScalarFunctionSignature::exact(vec![ubig_list(), ubig(), ubig()], ubig()),
            ScalarFunctionSignature::exact(vec![ubig_list(), ubig(), ubig(), int()], ubig()),
        ]
    }
}

// ----- h2_quantiles(buckets, qs, [p]) -> UBIGINT[] -----

pub struct H2QuantilesUdf;
impl VScalar for H2QuantilesUdf {
    type State = ();
    unsafe fn invoke(
        _: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let in_lv = input.list_vector(0);
        let in_qs = input.list_vector(1);
        let bch = in_lv.len();
        let qch = in_qs.len();
        let bdata = in_lv.child(bch).as_slice_with_len::<u64>(bch).to_vec();
        let qdata = in_qs.child(qch).as_slice_with_len::<f64>(qch).to_vec();
        let ps = read_p(input, 2, n);

        write_list_output(
            n,
            output,
            |r| {
                list_entry(&in_lv, r, bch)
                    .zip(list_entry(&in_qs, r, qch))
                    .map(|((boff, blen), (qoff, qlen))| {
                        (qlen, (boff, blen, qoff, qlen, ps[r]))
                    })
            },
            |(boff, blen, qoff, qlen, p), dst| {
                let total_count: u64 = if valid_p(p) {
                    bdata[boff..boff + blen].iter().sum()
                } else {
                    0
                };

                if total_count == 0 {
                    for slot in dst.iter_mut() { *slot = 0; }
                    return;
                }

                let mut qs_sorted: Vec<(usize, f64)> = (0..qlen)
                    .map(|i| (i, qdata[qoff + i].clamp(0.0, 1.0)))
                    .collect();
                qs_sorted.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

                let mut cumul: u64 = 0;
                let mut bi: usize = 0;
                for (orig_i, q) in qs_sorted {
                    let target: u64 = ((q * total_count as f64).ceil() as u64).max(1);
                    while bi < blen && cumul.saturating_add(bdata[boff + bi]) < target {
                        cumul = cumul.saturating_add(bdata[boff + bi]);
                        bi += 1;
                    }
                    if bi >= blen { bi = blen - 1; }
                    dst[orig_i] = h2_upper_p(bi as u32, p);
                }
            },
        );
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![
            ScalarFunctionSignature::exact(vec![ubig_list(), dbl_list()], ubig_list()),
            ScalarFunctionSignature::exact(vec![ubig_list(), dbl_list(), int()], ubig_list()),
        ]
    }
}

// ----- h2_combine(LIST<UBIGINT[]>) -> UBIGINT[] -----
// Element-wise sum. Output length = max input length encountered.

pub struct H2CombineUdf;
impl VScalar for H2CombineUdf {
    type State = ();
    unsafe fn invoke(
        _: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let outer = input.list_vector(0);
        let inner = outer.list_child();
        let leaf_n = inner.len();
        let leaf = inner.child(leaf_n).as_slice_with_len::<u64>(leaf_n).to_vec();

        // First pass: max inner length per outer row → output width per row.
        let mut row_widths = Vec::with_capacity(n);
        for r in 0..n {
            let (oo, ol) = outer.get_entry(r);
            let mut w = 0usize;
            for i in 0..ol {
                let (_, il) = inner.get_entry(oo + i);
                if il > w { w = il; }
            }
            row_widths.push(w);
        }
        let total_out: usize = row_widths.iter().sum();
        if total_out > MAX_BUCKET_COUNT * n {
            return Err(format!("h2_combine: output too large ({total_out})").into());
        }

        let mut out_lv = output.list_vector();
        let mut out_child = out_lv.child(total_out);
        let out_data = out_child.as_mut_slice_with_len::<u64>(total_out);
        for v in out_data.iter_mut() { *v = 0; }

        let mut dst_base = 0usize;
        for r in 0..n {
            let (oo, ol) = outer.get_entry(r);
            let w = row_widths[r];
            for i in 0..ol {
                let (io, il) = inner.get_entry(oo + i);
                for j in 0..il {
                    out_data[dst_base + j] =
                        out_data[dst_base + j].saturating_add(leaf[io + j]);
                }
            }
            out_lv.set_entry(r, dst_base, w);
            dst_base += w;
        }
        out_lv.set_len(total_out);
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![LogicalTypeHandle::list(&ubig_list())],
            ubig_list(),
        )]
    }
}

pub fn register_all(conn: &Connection) -> duckdb::Result<()> {
    conn.register_scalar_function::<H2LowerUdf>("h2_lower")?;
    conn.register_scalar_function::<H2UpperUdf>("h2_upper")?;
    conn.register_scalar_function::<H2MidUdf>("h2_midpoint")?;
    conn.register_scalar_function::<H2TotalUdf>("h2_total")?;
    conn.register_scalar_function::<H2DeltaUdf>("h2_delta")?;
    conn.register_scalar_function::<H2QuantileUdf>("h2_quantile")?;
    conn.register_scalar_function::<H2QuantilesUdf>("h2_quantiles")?;
    conn.register_scalar_function::<H2CountInRangeUdf>("h2_count_in_range")?;
    conn.register_scalar_function::<H2CombineUdf>("h2_combine")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- bucket-math invariants (pure functions, no DuckDB) ----------

    #[test]
    fn bucket_count_matches_formula() {
        for p in 2..=14u32 {
            assert_eq!(bucket_count(p), (N - p + 1) * (1 << p), "p={p}");
        }
    }

    #[test]
    fn agrees_with_rezolus_histogram_crate_at_p7() {
        // These expected values are lifted verbatim from the rezolus
        // `histogram` crate's own tests (histogram-1.2.0/src/config.rs:
        // `idx_to_lower_bound`, `idx_to_upper_bound`, `total_buckets`).
        // Any divergence here means we've fallen out of step with rezolus.
        assert_eq!(bucket_count(7), 7424);

        let lower = [(0, 0), (1, 1), (256, 256), (384, 512), (512, 1024),
                     (7423, 18_374_686_479_671_623_680u64)];
        for (idx, expected) in lower {
            assert_eq!(h2_lower_p(idx, 7), expected, "lower at idx={idx}");
        }

        let upper = [(0, 0), (1, 1), (256, 257), (384, 515), (512, 1031),
                     (7423, u64::MAX)];
        for (idx, expected) in upper {
            assert_eq!(h2_upper_p(idx, 7), expected, "upper at idx={idx}");
        }
    }

    #[test]
    fn first_bucket_holds_only_zero() {
        // Inclusive bounds: bucket 0 is [0, 0] — holds exactly the value 0.
        for p in 2..=14u32 {
            assert_eq!(h2_lower_p(0, p), 0, "p={p}");
            assert_eq!(h2_upper_p(0, p), 0, "p={p}");
        }
    }

    #[test]
    fn last_bucket_saturates_to_u64_max() {
        for p in 2..=14u32 {
            let last = bucket_count(p) - 1;
            assert_eq!(h2_upper_p(last, p), u64::MAX, "p={p}");
        }
    }

    #[test]
    fn buckets_tile_value_space() {
        // Inclusive bounds: upper(i) + 1 == lower(i+1) for every consecutive pair.
        for p in 2..=14u32 {
            let bc = bucket_count(p);
            let step = (bc / 4096).max(1) as usize;
            for i in (0..bc.saturating_sub(1)).step_by(step) {
                assert_eq!(
                    h2_upper_p(i, p) + 1,
                    h2_lower_p(i + 1, p),
                    "tiling broken at p={p}, i={i}"
                );
            }
        }
    }

    #[test]
    fn lower_is_strictly_monotone() {
        for p in 2..=14u32 {
            let bc = bucket_count(p);
            let step = (bc / 4096).max(1);
            let mut i = step;
            let mut prev = h2_lower_p(0, p);
            while i < bc {
                let cur = h2_lower_p(i, p);
                assert!(cur > prev, "non-monotone at p={p}, i={i}");
                prev = cur;
                i += step;
            }
        }
    }

    #[test]
    fn midpoint_lies_between_bounds() {
        for p in 2..=14u32 {
            let bc = bucket_count(p);
            let step = (bc / 4096).max(1) as usize;
            for i in (0..bc).step_by(step) {
                let lo = h2_lower_p(i, p);
                let hi = h2_upper_p(i, p);
                let mid = h2_mid_p(i, p);
                assert!(lo <= mid && mid <= hi, "p={p} i={i} lo={lo} mid={mid} hi={hi}");
            }
        }
    }

    // ---------- DuckDB-integrated UDF tests ----------

    fn fresh() -> Connection {
        let conn = Connection::open_in_memory().expect("open");
        register_all(&conn).expect("register UDFs");
        conn
    }

    fn one_u64(conn: &Connection, sql: &str) -> Option<u64> {
        conn.query_row(sql, [], |row| row.get::<_, Option<u64>>(0)).expect("query")
    }

    fn one_list(conn: &Connection, sql: &str) -> Option<Vec<u64>> {
        use duckdb::types::Value;
        conn.query_row(sql, [], |row| {
            let v: Value = row.get(0)?;
            Ok(match v {
                Value::Null => None,
                Value::List(items) | Value::Array(items) => Some(
                    items
                        .into_iter()
                        .map(|x| match x {
                            Value::UBigInt(n) => n,
                            _ => 0,
                        })
                        .collect(),
                ),
                _ => None,
            })
        })
        .expect("query")
    }

    #[test]
    fn h2_total_is_sum() {
        let conn = fresh();
        assert_eq!(one_u64(&conn, "SELECT h2_total([10,20,30,40]::UBIGINT[])"), Some(100));
        assert_eq!(one_u64(&conn, "SELECT h2_total([]::UBIGINT[])"), Some(0));
    }

    #[test]
    fn h2_delta_is_elementwise_saturating_sub() {
        let conn = fresh();
        assert_eq!(
            one_list(&conn, "SELECT h2_delta([100,200,300]::UBIGINT[], [10,20,30]::UBIGINT[])"),
            Some(vec![90, 180, 270])
        );
        // saturating: 5 - 100 == 0, not negative.
        assert_eq!(
            one_list(&conn, "SELECT h2_delta([5, 10]::UBIGINT[], [100, 5]::UBIGINT[])"),
            Some(vec![0, 5])
        );
    }

    #[test]
    fn h2_combine_sums_elementwise_with_widest_input_winning() {
        let conn = fresh();
        assert_eq!(
            one_list(&conn, "SELECT h2_combine([[1,2,3]::UBIGINT[], [10,20,30,40]::UBIGINT[]])"),
            Some(vec![11, 22, 33, 40])
        );
    }

    #[test]
    fn h2_quantile_overloads_agree() {
        // All four overload shapes should give the same result for an unbounded range.
        let conn = fresh();
        let sqls = [
            "SELECT h2_quantile([10,20,30,40]::UBIGINT[], 0.5)",
            "SELECT h2_quantile([10,20,30,40]::UBIGINT[], 0.5, 3)",
            "SELECT h2_quantile([10,20,30,40]::UBIGINT[], 0.5, 0::UBIGINT, 18446744073709551615::UBIGINT)",
            "SELECT h2_quantile([10,20,30,40]::UBIGINT[], 0.5, 0::UBIGINT, 18446744073709551615::UBIGINT, 3)",
        ];
        let answers: Vec<_> = sqls.iter().map(|s| one_u64(&conn, s)).collect();
        assert!(answers.windows(2).all(|w| w[0] == w[1]), "answers: {answers:?}");
    }

    #[test]
    fn h2_quantile_boundaries() {
        let conn = fresh();
        // [0,0,5,0,10]: q=0 falls in first non-empty bucket (idx 2 → inclusive upper 2),
        // q=1.0 falls in last (idx 4 → inclusive upper 4).
        assert_eq!(one_u64(&conn, "SELECT h2_quantile([0,0,5,0,10]::UBIGINT[], 0.0)"), Some(2));
        assert_eq!(one_u64(&conn, "SELECT h2_quantile([0,0,5,0,10]::UBIGINT[], 1.0)"), Some(4));
    }

    #[test]
    fn h2_quantile_empty_histogram_is_null() {
        let conn = fresh();
        assert_eq!(one_u64(&conn, "SELECT h2_quantile([0,0,0]::UBIGINT[], 0.5)"), None);
    }

    #[test]
    fn h2_count_in_range_full_range_equals_h2_total() {
        let conn = fresh();
        let buckets = "[1,2,3,4,5,6,7,8,9,10]::UBIGINT[]";
        let total = one_u64(&conn, &format!("SELECT h2_total({buckets})")).unwrap();
        let in_range = one_u64(
            &conn,
            &format!("SELECT h2_count_in_range({buckets}, 0::UBIGINT, 18446744073709551615::UBIGINT)"),
        )
        .unwrap();
        assert_eq!(in_range, total);
    }

    #[test]
    fn h2_quantiles_matches_singleton_quantile_per_q() {
        let conn = fresh();
        let multi = one_list(
            &conn,
            "SELECT h2_quantiles([10,20,30,40]::UBIGINT[], [0.5, 0.9]::DOUBLE[])",
        )
        .unwrap();
        let p50 = one_u64(&conn, "SELECT h2_quantile([10,20,30,40]::UBIGINT[], 0.5)").unwrap();
        let p90 = one_u64(&conn, "SELECT h2_quantile([10,20,30,40]::UBIGINT[], 0.9)").unwrap();
        assert_eq!(multi, vec![p50, p90]);
    }

    #[test]
    fn h2_lower_upper_midpoint_match_pure_functions() {
        let conn = fresh();
        for i in [0i32, 1, 5, 100, 200, 400] {
            let lo = one_u64(&conn, &format!("SELECT h2_lower({i})")).unwrap();
            let hi = one_u64(&conn, &format!("SELECT h2_upper({i})")).unwrap();
            let mid = one_u64(&conn, &format!("SELECT h2_midpoint({i})")).unwrap();
            assert_eq!(lo, h2_lower_p(i as u32, DEFAULT_P));
            assert_eq!(hi, h2_upper_p(i as u32, DEFAULT_P));
            assert_eq!(mid, h2_mid_p(i as u32, DEFAULT_P));
        }
    }

    #[test]
    fn h2_lower_returns_null_for_invalid_input() {
        let conn = fresh();
        assert_eq!(one_u64(&conn, "SELECT h2_lower(-1)"), None);
        assert_eq!(one_u64(&conn, "SELECT h2_lower(NULL::INTEGER)"), None);
        // bucket_count(3) = 496, so 1000 is out of range.
        assert_eq!(one_u64(&conn, "SELECT h2_lower(1000)"), None);
    }

    // ---------- randomized properties ----------

    /// Tiny deterministic LCG; avoids pulling in `rand`.
    struct Lcg(u64);
    impl Lcg {
        fn new(seed: u64) -> Self {
            Self(seed)
        }
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0
        }
    }

    fn ubigint_list_lit(xs: &[u64]) -> String {
        let inner = xs.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
        format!("[{inner}]::UBIGINT[]")
    }

    #[test]
    fn property_bucket_layout_at_random_indices() {
        let mut rng = Lcg::new(7);
        for _ in 0..400 {
            let p = 2 + (rng.next() % 13) as u32;
            let bc = bucket_count(p);
            let i = (rng.next() % bc as u64) as u32;
            let lo = h2_lower_p(i, p);
            let hi = h2_upper_p(i, p);
            let mid = h2_mid_p(i, p);
            assert!(lo <= mid && mid <= hi);
            if i + 1 < bc {
                // Inclusive convention: upper(i) + 1 == lower(i+1) (gap-free).
                assert_eq!(h2_upper_p(i, p) + 1, h2_lower_p(i + 1, p));
            }
        }
    }

    #[test]
    fn property_h2_combine_total_equals_sum_of_totals() {
        let conn = fresh();
        let mut rng = Lcg::new(0xC0FFEE);
        for _ in 0..30 {
            let n_hists = 1 + (rng.next() % 5) as usize;
            let mut hists: Vec<Vec<u64>> = Vec::new();
            for _ in 0..n_hists {
                let len = 1 + (rng.next() % 8) as usize;
                hists.push((0..len).map(|_| rng.next() % 1000).collect());
            }
            let outer = hists
                .iter()
                .map(|h| ubigint_list_lit(h))
                .collect::<Vec<_>>()
                .join(",");
            let expected: u64 = hists.iter().flat_map(|h| h.iter()).sum();
            let got = one_u64(&conn, &format!("SELECT h2_total(h2_combine([{outer}]))")).unwrap();
            assert_eq!(got, expected, "hists={hists:?}");
        }
    }

    #[test]
    fn property_h2_quantile_is_monotone_in_q() {
        let conn = fresh();
        let mut rng = Lcg::new(0xDEADBEEF);
        for trial in 0..30 {
            let len = 8 + (rng.next() % 16) as usize;
            let buckets: Vec<u64> = (0..len).map(|_| rng.next() % 100).collect();
            let lit = ubigint_list_lit(&buckets);
            let mut prev: Option<u64> = None;
            for q_pct in [0u32, 10, 25, 50, 75, 90, 99, 100] {
                let q = q_pct as f64 / 100.0;
                let cur = one_u64(&conn, &format!("SELECT h2_quantile({lit}, {q})"));
                if let (Some(c), Some(p)) = (cur, prev) {
                    assert!(c >= p, "trial {trial} q={q_pct}: {p} → {c}, hist={buckets:?}");
                }
                if cur.is_some() {
                    prev = cur;
                }
            }
        }
    }

    #[test]
    fn property_h2_delta_with_self_is_zero() {
        let conn = fresh();
        let mut rng = Lcg::new(42);
        for _ in 0..20 {
            let len = 1 + (rng.next() % 10) as usize;
            let buckets: Vec<u64> = (0..len).map(|_| rng.next() % 100).collect();
            let lit = ubigint_list_lit(&buckets);
            let r = one_list(&conn, &format!("SELECT h2_delta({lit}, {lit})"));
            assert_eq!(r, Some(vec![0; len]));
        }
    }
}
