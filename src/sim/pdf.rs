//! Discrete latency distribution for the `simulate` command.
//!
//! The user specifies the PDF as a comma-separated weight list, e.g.
//! `"0.5,0.25,0.25"`, meaning:
//!   - index 0: P(our arb tx lands on time)                = 0.50
//!   - index 1: P(it lands 1 swap/event late)              = 0.25
//!   - index 2: P(it lands 2 swaps/events late)            = 0.25
//!
//! Index = how many subsequent swap events execute against the target pool(s)
//! before our transaction does. The weights must sum to exactly 1.
//!
//! To keep with the "no floats in the money path" rule and to make the
//! sum-to-one check exact, weights are parsed into fixed-point integers scaled
//! by [`SCALE`] (1e9). Sampling draws a uniform integer in `[0, SCALE)`.

use crate::sim::rng::SplitMix64;

/// Fixed-point scale for probabilities (1.0 == 1e9).
pub const SCALE: u64 = 1_000_000_000;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PdfError {
    #[error("empty PDF")]
    Empty,
    #[error("weight {0:?} is not a valid non-negative decimal")]
    BadWeight(String),
    #[error("weights sum to {got} scaled units; must sum to exactly {SCALE}")]
    NotNormalized { got: u64 },
    #[error("too many fractional digits in {0:?} (max 9)")]
    TooPrecise(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatencyPdf {
    /// `weights[k]` = scaled probability that our tx lands `k` events late.
    weights: Vec<u64>,
    /// Cumulative weights for sampling (`cum[k]` = sum of weights[0..=k]).
    cumulative: Vec<u64>,
}

impl LatencyPdf {
    /// Parse `"0.5,0.25,0.25"` into a validated distribution.
    pub fn parse(s: &str) -> Result<Self, PdfError> {
        let parts: Vec<&str> = s.split(',').map(|p| p.trim()).filter(|p| !p.is_empty()).collect();
        if parts.is_empty() {
            return Err(PdfError::Empty);
        }
        let mut weights = Vec::with_capacity(parts.len());
        for p in parts {
            weights.push(parse_fixed(p)?);
        }
        Self::from_weights(weights)
    }

    /// Build from already-scaled weights (must sum to [`SCALE`]).
    pub fn from_weights(weights: Vec<u64>) -> Result<Self, PdfError> {
        if weights.is_empty() {
            return Err(PdfError::Empty);
        }
        let sum: u64 = weights.iter().sum();
        if sum != SCALE {
            return Err(PdfError::NotNormalized { got: sum });
        }
        let mut cumulative = Vec::with_capacity(weights.len());
        let mut acc = 0u64;
        for &w in &weights {
            acc += w;
            cumulative.push(acc);
        }
        Ok(Self { weights, cumulative })
    }

    /// Largest latency (in events) this PDF can produce.
    pub fn max_delay(&self) -> usize {
        self.weights.len() - 1
    }

    /// Scaled weight for `k` events late (0 if out of range).
    pub fn weight(&self, k: usize) -> u64 {
        self.weights.get(k).copied().unwrap_or(0)
    }

    /// Sample a delay (number of events late) using the provided RNG.
    pub fn sample(&self, rng: &mut SplitMix64) -> usize {
        let r = rng.below(SCALE);
        // First bucket whose cumulative weight strictly exceeds r.
        for (k, &c) in self.cumulative.iter().enumerate() {
            if r < c {
                return k;
            }
        }
        // Unreachable when sum == SCALE, but fall back to the last bucket.
        self.weights.len() - 1
    }
}

/// Parse a non-negative decimal like "0.25" or "1" into fixed-point scaled by
/// 1e9, with no floating point.
fn parse_fixed(s: &str) -> Result<u64, PdfError> {
    let bad = || PdfError::BadWeight(s.to_string());
    if s.is_empty() || s.starts_with('-') {
        return Err(bad());
    }
    let (int_part, frac_part) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    // Allow empty integer part ("" -> 0) only if there is a fractional part.
    let int_str = if int_part.is_empty() { "0" } else { int_part };
    if !int_str.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    if !frac_part.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    if frac_part.len() > 9 {
        return Err(PdfError::TooPrecise(s.to_string()));
    }
    let int_val: u64 = int_str.parse().map_err(|_| bad())?;
    // Right-pad the fractional part to 9 digits, then parse.
    let mut frac_padded = String::from(frac_part);
    while frac_padded.len() < 9 {
        frac_padded.push('0');
    }
    let frac_val: u64 = if frac_padded.is_empty() {
        0
    } else {
        frac_padded.parse().map_err(|_| bad())?
    };
    int_val
        .checked_mul(SCALE)
        .and_then(|v| v.checked_add(frac_val))
        .ok_or_else(bad)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_example() {
        let pdf = LatencyPdf::parse("0.5,0.25,0.25").unwrap();
        assert_eq!(pdf.weight(0), 500_000_000);
        assert_eq!(pdf.weight(1), 250_000_000);
        assert_eq!(pdf.weight(2), 250_000_000);
        assert_eq!(pdf.max_delay(), 2);
    }

    #[test]
    fn parses_always_on_time() {
        let pdf = LatencyPdf::parse("1").unwrap();
        assert_eq!(pdf.max_delay(), 0);
        assert_eq!(pdf.weight(0), SCALE);
    }

    #[test]
    fn parses_always_one_late() {
        let pdf = LatencyPdf::parse("0,1").unwrap();
        let mut rng = SplitMix64::new(123);
        for _ in 0..1000 {
            assert_eq!(pdf.sample(&mut rng), 1);
        }
    }

    #[test]
    fn rejects_unnormalized() {
        assert!(matches!(
            LatencyPdf::parse("0.5,0.25"),
            Err(PdfError::NotNormalized { .. })
        ));
        assert!(matches!(
            LatencyPdf::parse("0.6,0.6"),
            Err(PdfError::NotNormalized { .. })
        ));
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(LatencyPdf::parse(""), Err(PdfError::Empty)));
        assert!(matches!(
            LatencyPdf::parse("abc"),
            Err(PdfError::BadWeight(_))
        ));
        assert!(matches!(
            LatencyPdf::parse("-0.5,1.5"),
            Err(PdfError::BadWeight(_))
        ));
        assert!(matches!(
            LatencyPdf::parse("0.1234567891,..."),
            Err(PdfError::TooPrecise(_))
        ));
    }

    #[test]
    fn sampling_matches_weights_statistically() {
        let pdf = LatencyPdf::parse("0.5,0.25,0.25").unwrap();
        let mut rng = SplitMix64::new(99);
        let mut counts = [0u64; 3];
        let trials = 200_000u64;
        for _ in 0..trials {
            counts[pdf.sample(&mut rng)] += 1;
        }
        // Within ~2% of the nominal split.
        let frac0 = counts[0] as f64 / trials as f64;
        let frac1 = counts[1] as f64 / trials as f64;
        let frac2 = counts[2] as f64 / trials as f64;
        assert!((frac0 - 0.5).abs() < 0.02, "frac0={frac0}");
        assert!((frac1 - 0.25).abs() < 0.02, "frac1={frac1}");
        assert!((frac2 - 0.25).abs() < 0.02, "frac2={frac2}");
    }

    #[test]
    fn sampling_is_reproducible() {
        let pdf = LatencyPdf::parse("0.3,0.3,0.4").unwrap();
        let mut a = SplitMix64::new(2024);
        let mut b = SplitMix64::new(2024);
        let seq_a: Vec<usize> = (0..100).map(|_| pdf.sample(&mut a)).collect();
        let seq_b: Vec<usize> = (0..100).map(|_| pdf.sample(&mut b)).collect();
        assert_eq!(seq_a, seq_b);
    }
}
