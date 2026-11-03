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

// Research limits: bounded per-call storage and a process-wide, non-evicting compilation budget.
const MAX_ELEMENTS: usize = 1_048_576;
const MAX_SPECIALIZATIONS: usize = 16;
const MAX_MATRIX_MULTIPLIES: u64 = 16_777_216;

// Serialize the entire synchronous launch so this prototype has at most one compiler job.
static EXECUTION_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn admit_specialization(key: String) -> l3i::Result<()> {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static KEYS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let mut keys = KEYS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .map_err(|_| l3i::Error::runtime("specialization budget unavailable"))?;
    if keys.contains(&key) {
        return Ok(());
    }
    if keys.len() >= MAX_SPECIALIZATIONS {
        return Err(l3i::Error::runtime("specialization budget exhausted"));
    }
    keys.insert(key);
    Ok(())
}

fn validate_program(program: &str) -> l3i::Result<()> {
    if program.is_empty() || program.len() > 32 || !program.bytes().all(|op| matches!(op, b'*' | b'+' | b'2' | b'r')) {
        return Err(l3i::Error::runtime("invalid expression"));
    }
    Ok(())
}

// A checked copy severs the Luau buffer lifetime before any backend call. Stride and offset are
// byte counts; zero stride broadcasts one element, and output is always a fresh contiguous buffer.
fn copy_view(view: BufferView<'_>, offset: usize, stride: usize, count: usize) -> l3i::Result<Vec<u8>> {
    let bad = || l3i::Error::runtime("strided view exceeds buffer");
    if count == 0 {
        if offset > view.len() {
            return Err(bad());
        }
        return Ok(Vec::new());
    }
    let end = count
        .checked_sub(1)
        .and_then(|n| n.checked_mul(stride))
        .and_then(|n| offset.checked_add(n))
        .and_then(|n| n.checked_add(4))
        .ok_or_else(bad)?;
    if end > view.len() {
        return Err(bad());
    }
    let bytes = count.checked_mul(4).ok_or_else(bad)?;
    let mut owned = Vec::new();
    owned.try_reserve_exact(bytes).map_err(|_| bad())?;
    for index in 0..count {
        let mut element = [0; 4];
        view.read(offset + index * stride, &mut element)?;
        owned.extend_from_slice(&element);
    }
    Ok(owned)
}

fn run_owned(program: &str, a: &[u8], b: &[u8]) -> l3i::Result<NewBuffer> {
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
    if a.len() != b.len() || !a.len().is_multiple_of(4) {
        return Err(l3i::Error::runtime("invalid buffer sizes"));
    }
    if a.is_empty() {
        return Ok(NewBuffer(Vec::new()));
    }
    let n = a.len() / 4;
    if n > MAX_ELEMENTS {
        return Err(l3i::Error::runtime("allocation budget exceeded"));
    }
    let _gate = EXECUTION_GATE.lock().map_err(|_| l3i::Error::runtime("executor unavailable"))?;
    admit_specialization(format!("expression:{program}"))?;
    let count = u32::try_from(n).map_err(|_| l3i::Error::runtime("input too large"))?;
    if count > u32::MAX - 255 {
        return Err(l3i::Error::runtime("input too large"));
    }
    let client = CpuRuntime::client(&CpuDevice);
    let ah = client.create_from_slice(a);
    let bh = client.create_from_slice(b);
    let out = client.empty(a.len());
    // SAFETY: both allocations contain n f32 values and every indexed access is guarded.
    unsafe {
        expression::launch::<CpuRuntime>(
            &client,
            CubeCount::Static(count.div_ceil(256), 1, 1),
            CubeDim::new_1d(256),
            ArrayArg::from_raw_parts(ah, n),
            ArrayArg::from_raw_parts(bh, n),
            ArrayArg::from_raw_parts(out.clone(), n),
            ops,
        );
    }
    Ok(NewBuffer(client.read_one_unchecked(out).to_vec()))
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
        validate_program(&program)?;
        if a.len() != b.len() || !a.len().is_multiple_of(4) {
            return Err(l3i::Error::runtime("invalid buffer sizes"));
        }
        if a.len() / 4 > MAX_ELEMENTS {
            return Err(l3i::Error::runtime("allocation budget exceeded"));
        }
        run_owned(&program, &a.to_vec(), &b.to_vec())
    })?;
    let strided = runtime.bind_function(
        "dream.research.strided",
        |program: String, a: BufferView, b: BufferView, ao: i64, astep: i64, bo: i64, bstep: i64, count: i64| {
            validate_program(&program)?;
            let bad = || l3i::Error::runtime("invalid strided view");
            let count = usize::try_from(count).map_err(|_| bad())?;
            if count > MAX_ELEMENTS {
                return Err(bad());
            }
            let a = copy_view(
                a,
                usize::try_from(ao).map_err(|_| bad())?,
                usize::try_from(astep).map_err(|_| bad())?,
                count,
            )?;
            let b = copy_view(
                b,
                usize::try_from(bo).map_err(|_| bad())?,
                usize::try_from(bstep).map_err(|_| bad())?,
                count,
            )?;
            run_owned(&program, &a, &b)
        },
    )?;
    runtime.set_global("run_strided", &strided)?;
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
            if usize::try_from(an).map_err(|_| bad())? > MAX_ELEMENTS
                || usize::try_from(bn).map_err(|_| bad())? > MAX_ELEMENTS
                || usize::try_from(count).map_err(|_| bad())? > MAX_ELEMENTS
                || u64::from(count) * u64::from(inner) > MAX_MATRIX_MULTIPLIES
            {
                return Err(l3i::Error::runtime("matrix resource budget exceeded"));
            }
            let out_size = size(count)?;
            if count == 0 || inner == 0 {
                let mut result = Vec::new();
                result.try_reserve_exact(out_size).map_err(|_| l3i::Error::runtime("matrix allocation failed"))?;
                result.resize(out_size, 0);
                return Ok(NewBuffer(result));
            }
            let _gate = EXECUTION_GATE.lock().map_err(|_| l3i::Error::runtime("executor unavailable"))?;
            admit_specialization(format!("matrix:{rows}:{cols}:{inner}"))?;
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
        if a.len() != b.len() || !a.len().is_multiple_of(4) {
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

    let copy = runtime.bind_function("dream.research.copy_inputs", |a: BufferView, b: BufferView| {
        std::hint::black_box((a.to_vec(), b.to_vec()));
    })?;
    runtime.set_global("copy_inputs", &copy)?;
    let noop = runtime.bind_function("dream.research.noop", || ())?;
    runtime.set_global("noop", &noop)?;
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
