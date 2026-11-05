//! Row-stage and composition tests against oracles that share no code or formula with the kernels.
#![allow(clippy::many_single_char_names, clippy::cast_precision_loss, clippy::float_cmp)]
use crate::*;
use std::sync::Arc;

pub(crate) fn bytes_of(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}
pub(crate) fn floats(bytes: &[u8]) -> Vec<f32> {
    bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect()
}

/// A tensor `shape` over storage holding `values` at every `spacing`-th element, read through a
/// transposed walk when `transposed`: the same logical values in a very different memory layout.
pub(crate) fn laid_out(values: &[f32], rows: usize, cols: usize, spacing: usize, transposed: bool) -> Tensor {
    let mut storage = vec![f32::NAN; (rows * cols).max(1) * spacing];
    for row in 0..rows {
        for col in 0..cols {
            let at = if transposed { col * rows + row } else { row * cols + col };
            storage[at * spacing] = values[row * cols + col];
        }
    }
    let strides = if transposed { [spacing * 4, rows * spacing * 4] } else { [cols * spacing * 4, spacing * 4] };
    Tensor::from_storage(Arc::new(Vector(bytes_of(&storage))), Shape::Matrix(rows, cols), 0, strides).unwrap()
}
#[derive(Debug)]
pub(crate) struct Vector(pub(crate) Vec<u8>);
impl Storage for Vector {
    fn bytes(&self) -> &[u8] {
        &self.0
    }
}

#[test]
fn transpose_copies_bits_across_layouts_and_composes() {
    let budget = Budget::new(1 << 24);
    let domain = Domain::new(1024, 1024);
    for (rows, cols) in [(0, 3), (3, 0), (1, 1), (1, 7), (7, 1), (3, 5), (64, 33)] {
        let values: Vec<f32> =
            (0..rows * cols).map(|i| if i == 2 { f32::NAN } else { (i as f32 - 3.0) * -0.5 }).collect();
        let program =
            Expr::slot(0, Shape::Matrix(rows, cols), domain.clone()).unwrap().transpose().unwrap().compile().unwrap();
        assert_eq!(program.output_shape(), Shape::Matrix(cols, rows));
        for (spacing, transposed) in [(1, false), (2, false), (1, true), (3, true)] {
            let input = laid_out(&values, rows, cols, spacing, transposed);
            let result = floats(&program.run(&[&input], &budget).unwrap().readback().unwrap());
            for row in 0..rows {
                for col in 0..cols {
                    let (actual, expected) = (result[col * rows + row], values[row * cols + col]);
                    assert_eq!(actual.to_bits(), expected.to_bits(), "{rows}x{cols} ({row},{col})");
                }
            }
        }
    }
    // Twice is the identity, a product with its own transpose is symmetric, and neither needs more
    // than the planned temporaries.
    let values: Vec<f32> = (0..12).map(|i| i as f32 * 0.25 + 1.0).collect();
    let input = laid_out(&values, 3, 4, 1, false);
    let x = Expr::slot(0, Shape::Matrix(3, 4), domain).unwrap();
    let twice = x.transpose().unwrap().transpose().unwrap().compile().unwrap();
    assert_eq!(floats(&twice.run(&[&input], &budget).unwrap().readback().unwrap()), values);
    let gram = x.matmul(&x.transpose().unwrap()).unwrap().compile().unwrap();
    let product = floats(&gram.run(&[&input], &budget).unwrap().readback().unwrap());
    for i in 0..3 {
        for j in 0..3 {
            assert_eq!(product[i * 3 + j], product[j * 3 + i]);
        }
    }
    assert!(gram.native_source().is_err(), "lowering refuses what it cannot express");
    assert_eq!(budget.used(), 0);
}
