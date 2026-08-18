//! CPU-local embedding normalization benches (no Neo4j).
//!
//! Graph ranking and lock latency live in `mindreader-bench`.

use divan::{counter::ItemsCount, Bencher};
use mindreader::developer::embeddings::normalize_vector;

const DIMENSIONS: &[usize] = &[3, 256, 512, 1536, 3072, 4096];

fn main() {
    divan::main();
}

/// Non-zero finite vector of `dimension` so L2 normalization stays in the happy path.
fn input(dimension: usize) -> Vec<f64> {
    (0..dimension)
        .map(|index| (index % 31 + 1) as f64)
        .collect()
}

#[divan::bench(args = DIMENSIONS)]
/// Production L2-normalize across typical embedding widths (no Neo4j).
fn normalize_production(bencher: Bencher<'_, '_>, dimension: usize) {
    bencher
        .with_inputs(|| input(dimension))
        .counter(ItemsCount::new(dimension))
        .bench_local_values(|vector| normalize_vector(vector, dimension, "bench").unwrap());
}
