//! Summary statistics (min, median, p90, max) over measured trial samples.

use serde::Serialize;

/// Percentile summary in milliseconds (or bytes).
///
/// Formula on sorted samples `x[0] <= ... <= x[n - 1]`:
/// - `median`: exact middle element for odd `n`, arithmetic mean of the two middle
///   elements for even `n`.
/// - `p90`: nearest-rank 90th percentile, `x[ceil(0.90 * n) - 1]` (index 8 of 0..9
///   when `n == 10`).
/// - `max`: `x[n - 1]`.
#[derive(Clone, Debug, Serialize)]
pub struct Summary {
    pub n: usize,
    pub min: f64,
    pub median: f64,
    pub p90: f64,
    pub max: f64,
}

impl Summary {
    pub fn from_samples(samples: &[f64]) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }
        let mut sorted: Vec<f64> = samples.iter().copied().filter(|v| v.is_finite()).collect();
        if sorted.is_empty() {
            return None;
        }
        sorted.sort_by(f64::total_cmp);
        let n = sorted.len();
        let median = if n % 2 == 1 {
            sorted[n / 2]
        } else {
            (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
        };
        let p90_index = ((0.90 * n as f64).ceil() as usize)
            .saturating_sub(1)
            .min(n - 1);
        Some(Self {
            n,
            min: round2(sorted[0]),
            median: round2(median),
            p90: round2(sorted[p90_index]),
            max: round2(sorted[n - 1]),
        })
    }
}

pub fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}
