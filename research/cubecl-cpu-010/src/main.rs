use cubecl::cpu::{CpuDevice, CpuRuntime};
use cubecl::prelude::*;
use std::time::Instant;

#[cube(launch)]
fn fused(a: &Array<f32>, b: &Array<f32>, out: &mut Array<f32>) {
    if ABSOLUTE_POS < a.len() {
        let x = a[ABSOLUTE_POS] * b[ABSOLUTE_POS] + 2.0;
        if x > 0.0 {
            out[ABSOLUTE_POS] = x;
        } else {
            out[ABSOLUTE_POS] = 0.0;
        }
    }
}

fn main() {
    let runtime = l3i::Runtime::new().expect("create L3i runtime");
    runtime.exec("assert(2 + 2 == 4)").expect("execute Luau");
    let n = 65536usize;
    let a: Vec<f32> = (0..n).map(|i| f32::from(u16::try_from(i % 101).expect("remainder fits")) - 50.0).collect();
    let b: Vec<f32> = (0..n).map(|i| f32::from(u16::try_from(i % 13).expect("remainder fits")) - 6.0).collect();
    let start = Instant::now();
    let client = CpuRuntime::client(&CpuDevice);
    println!("initialize_us={}", start.elapsed().as_micros());
    let start = Instant::now();
    let ah = client.create_from_slice(f32::as_bytes(&a));
    let bh = client.create_from_slice(f32::as_bytes(&b));
    let out = client.empty(n * std::mem::size_of::<f32>());
    println!("upload_allocate_us={}", start.elapsed().as_micros());
    for iteration in 0..4 {
        let start = Instant::now();
        // SAFETY: each buffer holds n f32 values and the kernel guards its global index.
        unsafe {
            fused::launch::<CpuRuntime>(
                &client,
                CubeCount::Static(u32::try_from(n).expect("workload fits").div_ceil(256), 1, 1),
                CubeDim::new_1d(256),
                ArrayArg::from_raw_parts(ah.clone(), n),
                ArrayArg::from_raw_parts(bh.clone(), n),
                ArrayArg::from_raw_parts(out.clone(), n),
            );
        }
        let bytes = client.read_one_unchecked(out.clone());
        println!("iteration_{iteration}_dispatch_read_us={}", start.elapsed().as_micros());
        let actual = f32::from_bytes(&bytes);
        for i in 0..n {
            let expected = (a[i] * b[i] + 2.0).max(0.0);
            assert_eq!(actual[i], expected, "element {i}");
        }
    }
}
