//! The per-element reference interpreter, kept as the testing oracle and the benchmark baseline.
//! It dispatches every instruction for every output element through the checked `View::at`,
//! exactly as the first prototype did. It is deliberately not an optimized path.
use crate::Result;
use crate::compute::graph::Instruction;
use crate::compute::program::Program;
use crate::compute::reduction::{LANES, exp_nonpositive};
use crate::compute::{Budget, Tensor, View};
use std::sync::Arc;

fn relu(value: f32) -> f32 {
    if value <= 0.0 { 0.0 } else { value }
}

impl Program {
    /// Evaluates the graph one element at a time. Fused when it has no matmul.
    pub fn run_reference(&self, inputs: &[View<'_>], budget: &Arc<Budget>) -> Result<Tensor> {
        self.check(inputs)?;
        let shape = self.output_shape();
        let fused = !self.steps.iter().any(|step| {
            matches!(
                step.op,
                Instruction::MatMul(..)
                    | Instruction::SumRows(_)
                    | Instruction::Softmax(_)
                    | Instruction::Transpose(_)
                    | Instruction::Normalize(..)
            )
        });
        if fused {
            let mut output = budget.allocate(shape.elements()? * 4)?;
            self.reference_into(inputs, &mut output.bytes);
            return Tensor::contiguous(output, shape);
        }
        // Materialize each node, in the increasing-k order the numerical contract specifies.
        let mut values: Vec<Tensor> = Vec::with_capacity(self.steps.len());
        for step in &self.steps {
            if let Instruction::Slot(id) = step.op {
                let view = inputs[id];
                let (rows, cols) = view.shape.dims();
                let mut copy = budget.allocate(view.shape.elements()? * 4)?;
                for row in 0..rows {
                    for col in 0..cols {
                        let at = (row * cols + col) * 4;
                        copy.bytes[at..at + 4].copy_from_slice(&view.at(row, col).to_le_bytes());
                    }
                }
                values.push(Tensor::contiguous(copy, view.shape)?);
                continue;
            }
            let (rows, cols) = step.shape.dims();
            let mut output = budget.allocate(step.shape.elements()? * 4)?;
            let value = |id: usize| values[id].borrow();
            for row in 0..rows {
                for col in 0..cols {
                    let result = match step.op {
                        Instruction::Constant(v) => v,
                        Instruction::Add(a, b) => value(a).at(row, col) + value(b).at(row, col),
                        Instruction::Mul(a, b) => value(a).at(row, col) * value(b).at(row, col),
                        Instruction::Relu(a) => relu(value(a).at(row, col)),
                        Instruction::Normalize(a, epsilon, center) => {
                            normalize_reference(value(a), row, col, epsilon, center)
                        }
                        Instruction::Softmax(a) => {
                            let input = value(a);
                            let width = input.shape.dims().1;
                            let mut maximum = f64::NEG_INFINITY;
                            let mut positive = 0u32;
                            let mut nan = false;
                            for c in 0..width {
                                let v = input.at(row, c);
                                nan |= v.is_nan();
                                positive += u32::from(v == f32::INFINITY);
                                maximum = maximum.max(f64::from(v));
                            }
                            let v = input.at(row, col);
                            let probability = if nan {
                                f64::NAN
                            } else if positive != 0 {
                                if v == f32::INFINITY { 1.0 / f64::from(positive) } else { 0.0 }
                            } else if maximum == f64::NEG_INFINITY {
                                0.0
                            } else {
                                let denominator =
                                    lane_sum(width, |c| exp_nonpositive(f64::from(input.at(row, c)) - maximum));
                                exp_nonpositive(f64::from(v) - maximum) / denominator
                            };
                            #[allow(clippy::cast_possible_truncation)]
                            {
                                probability as f32
                            }
                        }
                        Instruction::Transpose(a) => value(a).at(col, row),
                        Instruction::SumRows(a) => {
                            let input = value(a);
                            (0..input.shape.dims().1).fold(0.0f32, |acc, c| acc + input.at(row, c))
                        }
                        Instruction::MatMul(a, b) => {
                            let (a, b) = (value(a), value(b));
                            let mut sum = 0.0f32;
                            for k in 0..a.shape.dims().1 {
                                sum += a.at(row, k) * b.at(k, col);
                            }
                            sum
                        }
                        Instruction::Slot(_) => unreachable!("input already materialized"),
                    };
                    let at = (row * cols + col) * 4;
                    output.bytes[at..at + 4].copy_from_slice(&result.to_le_bytes());
                }
            }
            values.push(Tensor::contiguous(output, step.shape)?);
        }
        Ok(values.pop().expect("nonempty compiled graph"))
    }

    /// The fused per-element loop, writing to caller storage: one instruction dispatch for each
    /// node of the graph for each output element.
    pub fn reference_into(&self, inputs: &[View<'_>], out: &mut [u8]) {
        let (rows, cols) = self.output_shape().dims();
        let mut registers = vec![0.0f32; self.steps.len()];
        for row in 0..rows {
            for col in 0..cols {
                for (i, step) in self.steps.iter().enumerate() {
                    registers[i] = match step.op {
                        Instruction::Slot(id) => inputs[id].at(row, col),
                        Instruction::Constant(v) => v,
                        Instruction::Add(a, b) => registers[a] + registers[b],
                        Instruction::Mul(a, b) => registers[a] * registers[b],
                        Instruction::Relu(a) => relu(registers[a]),
                        Instruction::MatMul(..)
                        | Instruction::SumRows(_)
                        | Instruction::Softmax(_)
                        | Instruction::Transpose(_)
                        | Instruction::Normalize(..) => {
                            unreachable!("fused graphs have no matmul")
                        }
                    };
                }
                let at = (row * cols + col) * 4;
                out[at..at + 4].copy_from_slice(&registers[registers.len() - 1].to_le_bytes());
            }
        }
    }
}

/// The row kernels' f64 accumulation contract, one element at a time.
fn lane_sum(width: usize, term: impl Fn(usize) -> f64) -> f64 {
    let mut totals = [0.0f64; LANES];
    for c in 0..width {
        totals[c % LANES] += term(c);
    }
    totals[0] + totals[1] + totals[2] + totals[3]
}

/// The first softmax kernel: the platform's `exp`, twice per element and with no scratch. Kept as
/// the baseline the current kernel is measured against, and as a check on it. Its bits depend on
/// the host's math library, which the current kernel's do not.
pub fn softmax_platform_exp(values: &[f32], out: &mut [f32]) {
    assert_eq!(values.len(), out.len());
    if values.is_empty() {
        return;
    }
    if values.iter().any(|v| v.is_nan()) {
        out.fill(f32::NAN);
        return;
    }
    let infinities = values.iter().filter(|v| **v == f32::INFINITY).count();
    if infinities != 0 {
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        let probability = (1.0 / infinities as f64) as f32;
        for (slot, value) in out.iter_mut().zip(values) {
            *slot = if *value == f32::INFINITY { probability } else { 0.0 };
        }
        return;
    }
    let max = values.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    if max == f32::NEG_INFINITY {
        out.fill(0.0);
        return;
    }
    let max = f64::from(max);
    let total: f64 = values.iter().map(|v| (f64::from(*v) - max).exp()).sum();
    for (slot, value) in out.iter_mut().zip(values) {
        #[allow(clippy::cast_possible_truncation)]
        {
            *slot = ((f64::from(*value) - max).exp() / total) as f32;
        }
    }
}

fn normalize_reference(input: View<'_>, row: usize, col: usize, epsilon: f64, center: bool) -> f32 {
    let width = input.shape.dims().1;
    #[allow(clippy::cast_precision_loss)]
    let count = width as f64;
    let mean = if center { lane_sum(width, |c| f64::from(input.at(row, c))) / count } else { 0.0 };
    let variance = lane_sum(width, |c| {
        let d = f64::from(input.at(row, c)) - mean;
        d * d
    });
    #[allow(clippy::cast_possible_truncation)]
    {
        ((f64::from(input.at(row, col)) - mean) / (variance / count + epsilon).sqrt()) as f32
    }
}
