//! The per-element reference interpreter, kept as the testing oracle and the benchmark baseline.
//! It dispatches every instruction for every output element through the checked `View::at`,
//! exactly as the first prototype did. It is deliberately not an optimized path.
use crate::graph::Instruction;
use crate::program::Program;
use crate::{Budget, Tensor, View};
use l3i::Result;
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
                                let mut denominator = 0.0;
                                for c in 0..width {
                                    denominator += (f64::from(input.at(row, c)) - maximum).exp();
                                }
                                (f64::from(v) - maximum).exp() / denominator
                            };
                            #[allow(clippy::cast_possible_truncation)]
                            {
                                probability as f32
                            }
                        }
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

fn normalize_reference(input: View<'_>, row: usize, col: usize, epsilon: f64, center: bool) -> f32 {
    let width = input.shape.dims().1;
    #[allow(clippy::cast_precision_loss)]
    let count = width as f64;
    let mut mean = 0.0;
    if center {
        for c in 0..width {
            mean += f64::from(input.at(row, c));
        }
        mean /= count;
    }
    let mut variance = 0.0;
    for c in 0..width {
        let d = f64::from(input.at(row, c)) - mean;
        variance += d * d;
    }
    #[allow(clippy::cast_possible_truncation)]
    {
        ((f64::from(input.at(row, col)) - mean) / (variance / count + epsilon).sqrt()) as f32
    }
}
