//! A small synthetic transformer-style forward pass, as one graph: layer normalization, three
//! projections, scaled dot-product attention, a residual, a rectified feed-forward layer and another
//! residual, per block. It exists to profile realistic stage mixes and to test composition. The
//! weights are deterministic noise, so it says nothing about model quality.
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
use crate::{Domain, Expr, Shape};
use l3i::Result;
use std::sync::Arc;

/// The shape of one forward pass.
#[derive(Clone, Copy, Debug)]
pub struct Model {
    pub tokens: usize,
    pub width: usize,
    pub blocks: usize,
}

/// Weight matrices per block: query, key, value, output, and the two feed-forward layers.
pub const WEIGHTS_PER_BLOCK: usize = 6;
const EPSILON: f64 = 1e-5;

impl Model {
    /// Width of the feed-forward layer's hidden state.
    #[must_use]
    pub fn hidden(&self) -> usize {
        self.width * 2
    }
    /// Input shapes by slot: the tokens, then each block's weights.
    #[must_use]
    pub fn slots(&self) -> Vec<Shape> {
        let (d, f) = (self.width, self.hidden());
        let mut slots = vec![Shape::Matrix(self.tokens, d)];
        for _ in 0..self.blocks {
            slots.extend([
                Shape::Matrix(d, d),
                Shape::Matrix(d, d),
                Shape::Matrix(d, d),
                Shape::Matrix(d, d),
                Shape::Matrix(d, f),
                Shape::Matrix(f, d),
            ]);
        }
        slots
    }
    /// The whole pass as one expression over [`Self::slots`].
    pub fn graph(&self, domain: &Arc<Domain>) -> Result<Expr> {
        let slots = self.slots();
        let slot = |id: usize| Expr::slot(id, slots[id], domain.clone());
        let scale = Expr::constant(1.0 / (self.width as f32).sqrt(), domain.clone())?;
        let mut x = slot(0)?;
        for block in 0..self.blocks {
            let weight = |index: usize| slot(1 + block * WEIGHTS_PER_BLOCK + index);
            let normalized = x.normalize(EPSILON, true)?;
            let (query, key) = (normalized.matmul(&weight(0)?)?, normalized.matmul(&weight(1)?)?);
            let value = normalized.matmul(&weight(2)?)?;
            let attention = query.matmul(&key.transpose()?)?.mul(&scale)?.softmax()?;
            let attended = attention.matmul(&value)?.matmul(&weight(3)?)?;
            let residual = x.add(&attended)?;
            let hidden = residual.normalize(EPSILON, true)?.matmul(&weight(4)?)?.relu()?;
            x = residual.add(&hidden.matmul(&weight(5)?)?)?;
        }
        Ok(x)
    }
    /// Deterministic inputs for [`Self::slots`], little-endian f32 bytes: noise scaled so activations
    /// stay moderate, computed the same way in `bench-shapes.jsl`.
    #[must_use]
    pub fn inputs(&self) -> Vec<Vec<u8>> {
        self.slots()
            .iter()
            .enumerate()
            .map(|(slot, shape)| {
                let (rows, cols) = shape.dims();
                let scale = if slot == 0 { 1.0 } else { 1.0 / (cols as f64).sqrt() };
                (0..rows * cols).flat_map(|index| ((noise(slot, index) * scale) as f32).to_le_bytes()).collect()
            })
            .collect()
    }
}

/// A value in [-1, 1), from an integer hash that Luau's doubles reproduce exactly.
#[must_use]
pub fn noise(slot: usize, index: usize) -> f64 {
    let mixed = ((index + 1) * 40_503 + (slot + 1) * 69_069) % 65_521;
    let mixed = (mixed * mixed + 12_345) % 65_521;
    f64::from(u32::try_from(mixed).expect("below 65,521")) / 32_760.5 - 1.0
}
