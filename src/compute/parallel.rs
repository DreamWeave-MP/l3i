//! Sharing a run among a host's workers changes how fast it is and nothing else.
#![allow(clippy::many_single_char_names, clippy::cast_precision_loss)]
use super::adversarial::{bytes_of, laid_out};
use crate::compute::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// One scoped thread per task, as simple a host as there can be.
#[derive(Debug)]
struct Threads(usize);
impl Executor for Threads {
    fn workers(&self) -> usize {
        self.0
    }
    fn run(&self, tasks: usize, task: &(dyn Fn(usize) + Sync)) {
        std::thread::scope(|scope| {
            for index in 0..tasks {
                scope.spawn(move || task(index));
            }
        });
    }
}
/// Runs tasks backward on one thread, to show order can't matter.
#[derive(Debug)]
struct Backward(usize);
impl Executor for Backward {
    fn workers(&self) -> usize {
        self.0
    }
    fn run(&self, tasks: usize, task: &(dyn Fn(usize) + Sync)) {
        for index in (0..tasks).rev() {
            task(index);
        }
    }
}
#[derive(Debug)]
struct Careless;
impl Executor for Careless {
    fn workers(&self) -> usize {
        8
    }
    fn run(&self, tasks: usize, task: &(dyn Fn(usize) + Sync)) {
        for index in 1..tasks {
            task(index);
        }
    }
}
/// Runs each task twice, which a host shouldn't, and asks for tasks that don't exist.
#[derive(Debug)]
struct Overeager;
impl Executor for Overeager {
    fn workers(&self) -> usize {
        8
    }
    fn run(&self, tasks: usize, task: &(dyn Fn(usize) + Sync)) {
        for index in 0..tasks + 3 {
            task(index);
            task(index);
        }
    }
}
#[derive(Debug)]
struct Counting(AtomicUsize, usize);
impl Executor for Counting {
    fn workers(&self) -> usize {
        self.1
    }
    fn run(&self, tasks: usize, task: &(dyn Fn(usize) + Sync)) {
        self.0.fetch_add(tasks, Ordering::Relaxed);
        for index in 0..tasks {
            task(index);
        }
    }
}

fn limits() -> Limits {
    Limits { elements: 1 << 24, work: 1 << 36 }
}
fn values(count: usize, seed: f32) -> Vec<f32> {
    (0..count).map(|i| ((i * 7 + 3) % 113) as f32 * 0.037 * seed - 1.9).collect()
}

/// Products big enough to share, either side of every split, in both modes and with weights packed
/// either way: bit-identical to one thread, in any task order.
#[test]
fn a_product_shared_among_workers_matches_one_thread_to_the_bit() {
    let domain = Domain::with_limits(64, 64, limits());
    let budget = Budget::with_elements(1 << 30, 1 << 24);
    for (m, k, n) in
        [(200, 128, 128), (97, 300, 80), (48, 400, 256), (49, 400, 256), (513, 70, 64), (300, 2000, 8), (5000, 512, 1)]
    {
        let (a, b) = (values(m * k, 1.0), values(k * n, 0.7));
        let (shape_a, shape_b) = (Shape::Matrix(m, k), Shape::Matrix(k, n));
        let ta = Tensor::import(&bytes_of(&a), shape_a, &budget).unwrap();
        let tb = Tensor::import(&bytes_of(&b), shape_b, &budget).unwrap();
        let graph = Expr::slot(0, shape_a, domain.clone())
            .unwrap()
            .matmul(&Expr::slot(1, shape_b, domain.clone()).unwrap())
            .unwrap();
        let weights = [
            (ta.clone(), tb.clone()),
            (ta.clone(), tb.prepare_right(&budget).unwrap()),
            (ta.prepare_left(&budget).unwrap(), tb.clone()),
        ];
        for numeric in [Numeric::Strict, Numeric::Fast] {
            let program = graph.compile_with(numeric).unwrap();
            for (left, right) in &weights {
                let views = [left.borrow(), right.borrow()];
                let mut want = vec![0u8; m * n * 4];
                program.run_into(&views, &mut want, &budget).unwrap();
                for executor in [&Threads(4) as &dyn Executor, &Backward(3), &Backward(64)] {
                    let mut out = vec![0xa5u8; m * n * 4];
                    program.run_into_parallel(&views, &mut out, &budget, executor).unwrap();
                    assert!(out == want, "{m}x{k}x{n} {numeric:?} {executor:?}");
                }
            }
        }
    }
}

#[test]
fn elementwise_work_shared_among_workers_matches_one_thread_to_the_bit() {
    let domain = Domain::with_limits(64, 64, limits());
    let budget = Budget::with_elements(1 << 30, 1 << 24);
    for (rows, cols) in [(1usize, 400_000usize), (800, 800), (513, 1023)] {
        let shape = Shape::Matrix(rows, cols);
        let count = rows * cols;
        let (x, y) = (values(count, 1.0), values(count, 0.3));
        let plain = laid_out(&x, rows, cols, 1, false);
        let walked = laid_out(&y, rows, cols, 2, true);
        let (p, q) = (Expr::slot(0, shape, domain.clone()).unwrap(), Expr::slot(1, shape, domain.clone()).unwrap());
        let half = Expr::constant(0.5, domain.clone()).unwrap();
        let graph = p.mul(&half).unwrap().add(&q).unwrap().relu().unwrap().mul(&p).unwrap().add(&q).unwrap();
        for numeric in [Numeric::Strict, Numeric::Fast] {
            let program = graph.compile_with(numeric).unwrap();
            let views = [plain.borrow(), walked.borrow()];
            let mut want = vec![0u8; count * 4];
            program.run_into(&views, &mut want, &budget).unwrap();
            for executor in [&Threads(4) as &dyn Executor, &Backward(7)] {
                let mut out = vec![0xa5u8; count * 4];
                program.run_into_parallel(&views, &mut out, &budget, executor).unwrap();
                assert!(out == want, "{rows}x{cols} {numeric:?} {executor:?}");
            }
        }
    }
}

/// Small work never reaches the workers, so a host's scheduling costs it nothing.
#[test]
fn work_too_small_to_share_stays_on_the_calling_thread() {
    let domain = Domain::new(64, 64);
    let budget = Budget::new(1 << 24);
    let executor = Counting(AtomicUsize::new(0), 8);
    let shape = Shape::Matrix(64, 64);
    let tensor = Tensor::import(&bytes_of(&values(64 * 64, 1.0)), shape, &budget).unwrap();
    let slot = Expr::slot(0, shape, domain).unwrap();
    for program in [slot.matmul(&slot).unwrap().compile().unwrap(), slot.add(&slot).unwrap().compile().unwrap()] {
        let mut out = vec![0u8; 64 * 64 * 4];
        program.run_into_parallel(&[tensor.borrow()], &mut out, &budget, &executor).unwrap();
    }
    assert_eq!(executor.0.load(Ordering::Relaxed), 0);
    // One worker is no sharing either, however large the product.
    let large = Budget::with_elements(1 << 28, 1 << 24);
    let domain = Domain::with_limits(64, 64, limits());
    let shape = Shape::Matrix(256, 256);
    let tensor = Tensor::import(&bytes_of(&values(256 * 256, 1.0)), shape, &large).unwrap();
    let slot = Expr::slot(0, shape, domain).unwrap();
    let program = slot.matmul(&slot).unwrap().compile().unwrap();
    let mut out = vec![0u8; 256 * 256 * 4];
    let one = Counting(AtomicUsize::new(0), 1);
    program.run_into_parallel(&[tensor.borrow()], &mut out, &large, &one).unwrap();
    assert_eq!(one.0.load(Ordering::Relaxed), 0);
    let two = Counting(AtomicUsize::new(0), 2);
    program.run_into_parallel(&[tensor.borrow()], &mut out, &large, &two).unwrap();
    assert_eq!(two.0.load(Ordering::Relaxed), 2, "large enough, and it does share");
}

fn shareable() -> (Program, Tensor, Arc<Budget>) {
    let domain = Domain::with_limits(64, 64, limits());
    let budget = Budget::with_elements(1 << 28, 1 << 24);
    let shape = Shape::Matrix(256, 256);
    let tensor = Tensor::import(&bytes_of(&values(256 * 256, 1.0)), shape, &budget).unwrap();
    let slot = Expr::slot(0, shape, domain).unwrap();
    (slot.matmul(&slot).unwrap().compile().unwrap(), tensor, budget)
}

/// Every worker's scratch is under its charge and in hand before anything is written, so failing on
/// the last worker's still leaves the output alone.
#[test]
fn a_refused_worker_leaves_the_output_untouched() {
    let (program, tensor, _) = shareable();
    let view = [tensor.borrow()];
    let mut want = vec![0u8; 256 * 256 * 4];
    let executor = Backward(4);
    let room = Budget::new(1 << 26);
    program.run_into_parallel(&view, &mut want, &room, &executor).unwrap();
    let peak = room.peak();
    assert!(peak > 0);
    for limit in (0..peak).step_by(peak / 64) {
        let budget = Budget::new(limit);
        let mut out = vec![0x5au8; want.len()];
        assert!(program.run_into_parallel(&view, &mut out, &budget, &executor).is_err(), "{limit} of {peak}");
        assert!(out.iter().all(|byte| *byte == 0x5a), "{limit} of {peak}: wrote before failing");
        assert_eq!(budget.used(), 0);
    }
    for fail_at in 0..8 {
        let budget = Budget::new(1 << 26);
        let mut out = vec![0x5au8; want.len()];
        let mut seen = 0;
        let result = with_allocation_hook(
            |_| {
                seen += 1;
                seen - 1 != fail_at
            },
            || program.run_into_parallel(&view, &mut out, &budget, &executor),
        );
        assert_eq!(budget.used(), 0);
        if result.is_err() {
            assert!(out.iter().all(|byte| *byte == 0x5a), "request {fail_at}: wrote before failing");
        } else {
            assert_eq!(out, want);
        }
    }
    let budget = Budget::new(peak);
    let mut out = vec![0u8; want.len()];
    program.run_into_parallel(&view, &mut out, &budget, &executor).unwrap();
    assert!(out == want, "the peak is enough");
}

#[test]
fn a_host_that_skips_a_task_gets_an_error_and_one_that_repeats_gets_the_answer() {
    let (program, tensor, budget) = shareable();
    let mut out = vec![0u8; 256 * 256 * 4];
    assert!(program.run_into_parallel(&[tensor.borrow()], &mut out, &budget, &Careless).is_err());
    let mut want = vec![0u8; 256 * 256 * 4];
    program.run_into(&[tensor.borrow()], &mut want, &budget).unwrap();
    program.run_into_parallel(&[tensor.borrow()], &mut out, &budget, &Overeager).unwrap();
    assert_eq!(out, want);
}
