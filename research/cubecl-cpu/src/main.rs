use cubecl_runtime::runtime::Runtime;
use cubecl::cpu::{CpuDevice, CpuRuntime};
use cubecl::prelude::*;
use std::time::Instant;

#[cube(launch_unchecked)]
fn fused(a: &[f32], b: &[f32], out: &mut [f32]) {
    if ABSOLUTE_POS < a.len() {
        let x = a[ABSOLUTE_POS] * b[ABSOLUTE_POS] + 2.0;
        out[ABSOLUTE_POS] = if x > 0.0 { x } else { 0.0f32 };
    }
}

fn main() {
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
        // SAFETY: all three allocations contain n f32 elements and the kernel checks its index.
        unsafe {
            fused::launch_unchecked(
                &client,
                CubeCount::Static(u32::try_from(n).expect("workload fits").div_ceil(256), 1, 1),
                CubeDim::new_1d(256),
                BufferArg::from_raw_parts(ah.clone(), n),
                BufferArg::from_raw_parts(bh.clone(), n),
                BufferArg::from_raw_parts(out.clone(), n),
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
