//! Isolated predefined fused-kernel experiment with persistent CPU backend allocations.
use cubecl::cpu::{CpuDevice, CpuRuntime};
use cubecl::prelude::*;
use std::time::Instant;

#[cube(launch)]
fn fused(a: &Array<f32>, b: &Array<f32>, out: &mut Array<f32>) {
    if ABSOLUTE_POS < a.len() {
        let value = a[ABSOLUTE_POS] + b[ABSOLUTE_POS] + 2.0;
        if value <= 0.0 {
            out[ABSOLUTE_POS] = 0.0;
        } else {
            out[ABSOLUTE_POS] = value;
        }
    }
}

fn main() {
    let client = CpuRuntime::client(&CpuDevice);
    for n in [1024usize, 65536] {
        let a: Vec<f32> = (0..n).map(|i| f32::from(u16::try_from(i % 31).unwrap()) - 15.0).collect();
        let b: Vec<f32> = (0..n).map(|i| f32::from(u16::try_from(i % 17).unwrap()) - 8.0).collect();
        let started = Instant::now();
        let ah = client.create_from_slice(f32::as_bytes(&a));
        let bh = client.create_from_slice(f32::as_bytes(&b));
        let out = client.empty(n * std::mem::size_of::<f32>());
        let setup = started.elapsed();
        let launch = || {
            // SAFETY: allocations each contain n f32 values; the kernel guards its global index.
            unsafe {
                fused::launch::<CpuRuntime>(
                    &client,
                    CubeCount::Static(u32::try_from(n).unwrap().div_ceil(256), 1, 1),
                    CubeDim::new_1d(256),
                    ArrayArg::from_raw_parts(ah.clone(), n),
                    ArrayArg::from_raw_parts(bh.clone(), n),
                    ArrayArg::from_raw_parts(out.clone(), n),
                );
            }
            cubecl::future::block_on(client.sync()).expect("CPU synchronization");
        };
        let started = Instant::now();
        launch();
        let first = started.elapsed();
        let started = Instant::now();
        for _ in 0..20 {
            launch();
        }
        let warm = started.elapsed() / 20;
        let started = Instant::now();
        let bytes = client.read_one_unchecked(out);
        let read = started.elapsed();
        let actual = f32::from_bytes(&bytes);
        let mut reference = vec![0f32; n];
        let started = Instant::now();
        for _ in 0..20 {
            for ((dst, x), y) in reference.iter_mut().zip(&a).zip(&b) {
                let value = *x + *y + 2.0;
                *dst = if value <= 0.0 { 0.0 } else { value };
            }
            std::hint::black_box(&reference);
        }
        let native = started.elapsed() / 20;
        assert_eq!(actual, reference, "fused output");
        println!(
            "n={n} setup_us={} first_sync_us={} warm_sync_us={} readback_us={} native_preallocated_us={}",
            setup.as_micros(),
            first.as_micros(),
            warm.as_micros(),
            read.as_micros(),
            native.as_micros()
        );
    }
}
