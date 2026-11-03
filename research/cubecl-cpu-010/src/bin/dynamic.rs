use cubecl::cpu::{CpuDevice, CpuRuntime};
use cubecl::prelude::*;
use l3i::convert::{BufferView, NewBuffer};

#[allow(clippy::needless_pass_by_value, clippy::needless_range_loop, clippy::assign_op_pattern, clippy::collapsible_if)]
#[cube(launch)]
fn expression(a: &Array<f32>, b: &Array<f32>, out: &mut Array<f32>, #[comptime] ops: Vec<u32>) {
    if ABSOLUTE_POS < a.len() {
        let mut value = a[ABSOLUTE_POS];
        #[unroll]
        for i in 0..ops.len() {
            let op = comptime!(ops[i]);
            if comptime!(op == 1) {
                value = value * b[ABSOLUTE_POS];
            } else if comptime!(op == 2) {
                value = value + b[ABSOLUTE_POS];
            } else if comptime!(op == 3) {
                value = value + 2.0;
            } else if comptime!(op == 4) {
                if value <= 0.0 {
                    value = 0.0;
                }
            }
        }
        out[ABSOLUTE_POS] = value;
    }
}

#[cube(launch)]
fn matrix(
    a: &Array<f32>,
    b: &Array<f32>,
    out: &mut Array<f32>,
    #[comptime] rows: usize,
    #[comptime] cols: usize,
    #[comptime] inner: usize,
) {
    let index = ABSOLUTE_POS;
    if index < rows * cols {
        let row = index / cols;
        let col = index % cols;
        let mut sum = 0.0f32;
        for k in 0..inner {
            sum += a[row * inner + k] * b[k * cols + col];
        }
        out[index] = sum;
    }
}

#[allow(clippy::too_many_lines)] // Keeps the experimental bindings together.
fn main() -> l3i::Result<()> {
    if cfg!(target_endian = "big") {
        return Err(l3i::Error::runtime("the experimental CPU bridge requires little-endian storage"));
    }
    let plan = l3i::extension::RuntimePlan::builder()
        .policy(l3i::extension::RuntimePolicy::new().compat_global("@dream/data", "data"))
        .extension(l3i::data::DataExtension)
        .finalize()?;
    let runtime = l3i::Runtime::from_plan(&plan)?;
    let run = runtime.bind_function("dream.research.run", |program: String, a: BufferView, b: BufferView| {
        if program.is_empty() || program.len() > 32 {
            return Err(l3i::Error::runtime("invalid expression length"));
        }
        let ops: Vec<u32> = program
            .bytes()
            .map(|op| match op {
                b'*' => Ok(1),
                b'+' => Ok(2),
                b'2' => Ok(3),
                b'r' => Ok(4),
                _ => Err(l3i::Error::runtime("unknown operation")),
            })
            .collect::<l3i::Result<_>>()?;
        if ops.is_empty() || ops.len() > 32 || a.len() != b.len() || a.len() % 4 != 0 {
            return Err(l3i::Error::runtime("invalid expression or buffer sizes"));
        }
        if a.is_empty() {
            return Ok(NewBuffer(Vec::new()));
        }
        let n = a.len() / 4;
        let count = u32::try_from(n).map_err(|_| l3i::Error::runtime("input too large"))?;
        if count > u32::MAX - 255 {
            return Err(l3i::Error::runtime("input too large"));
        }
        let cubes = count.div_ceil(256);
        let client = CpuRuntime::client(&CpuDevice);
        let ah = client.create_from_slice(&a.to_vec());
        let bh = client.create_from_slice(&b.to_vec());
        let out = client.empty(a.len());
        // SAFETY: buffers have identical sizes, each divisible by four, and every access is guarded.
        unsafe {
            expression::launch::<CpuRuntime>(
                &client,
                CubeCount::Static(cubes, 1, 1),
                CubeDim::new_1d(256),
                ArrayArg::from_raw_parts(ah, n),
                ArrayArg::from_raw_parts(bh, n),
                ArrayArg::from_raw_parts(out.clone(), n),
                ops,
            );
        }
        Ok(NewBuffer(client.read_one_unchecked(out).to_vec()))
    })?;
    let matmul = runtime.bind_function(
        "dream.research.matmul",
        |a: BufferView, b: BufferView, rows: i64, cols: i64, inner: i64| {
            let bad = || l3i::Error::runtime("invalid matrix dimensions or buffer sizes");
            let rows = u32::try_from(rows).map_err(|_| bad())?;
            let cols = u32::try_from(cols).map_err(|_| bad())?;
            let inner = u32::try_from(inner).map_err(|_| bad())?;
            let an = rows.checked_mul(inner).ok_or_else(bad)?;
            let bn = inner.checked_mul(cols).ok_or_else(bad)?;
            let count = rows.checked_mul(cols).filter(|v| *v <= u32::MAX - 255).ok_or_else(bad)?;
            let size = |v: u32| usize::try_from(v).ok().and_then(|v| v.checked_mul(4)).ok_or_else(bad);
            if a.len() != size(an)? || b.len() != size(bn)? {
                return Err(bad());
            }
            let out_size = size(count)?;
            if count == 0 || inner == 0 {
                return Ok(NewBuffer(vec![0; out_size]));
            }
            let client = CpuRuntime::client(&CpuDevice);
            let ah = client.create_from_slice(&a.to_vec());
            let bh = client.create_from_slice(&b.to_vec());
            let out = client.empty(out_size);
            // SAFETY: checked products match every allocation; the launch bounds guard every output,
            // and row, column and reduction indices stay inside the validated inputs.
            unsafe {
                matrix::launch::<CpuRuntime>(
                    &client,
                    CubeCount::Static(count.div_ceil(256), 1, 1),
                    CubeDim::new_1d(256),
                    ArrayArg::from_raw_parts(ah, an as usize),
                    ArrayArg::from_raw_parts(bh, bn as usize),
                    ArrayArg::from_raw_parts(out.clone(), count as usize),
                    rows as usize,
                    cols as usize,
                    inner as usize,
                );
            }
            Ok(NewBuffer(client.read_one_unchecked(out).to_vec()))
        },
    )?;
    runtime.set_global("matmul", &matmul)?;
    let started = std::time::Instant::now();
    let clock = runtime.bind_function("dream.research.clock", move || started.elapsed().as_secs_f64())?;
    runtime.set_global("clock", &clock)?;
    let native = runtime.bind_function("dream.research.native_add", |a: BufferView, b: BufferView| {
        if a.len() != b.len() || a.len() % 4 != 0 {
            return Err(l3i::Error::runtime("invalid buffer sizes"));
        }
        let aa = a.to_vec();
        let bb = b.to_vec();
        let mut result = Vec::with_capacity(aa.len());
        for (x, y) in aa.as_chunks::<4>().0.iter().zip(bb.as_chunks::<4>().0.iter()) {
            let x = f32::from_le_bytes(*x);
            let y = f32::from_le_bytes(*y);
            result.extend_from_slice(&(x + y).to_le_bytes());
        }
        Ok(NewBuffer(result))
    })?;
    runtime.set_global("native_add", &native)?;
    runtime.set_global("run_expression", &run)?;
    if std::env::args().any(|arg| arg == "--bench") {
        runtime.exec(include_str!("../../bench.jsl"))?;
    } else {
        runtime.exec(include_str!("../../dynamic.jsl"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn expression_and_matrix_survive_runtime_recreation() {
        super::main().expect("first runtime");
        super::main().expect("second runtime");
    }
}
