//! Fixed-width integer helpers used across the AMM simulators.
//!
//! All on-chain AMM math is done in unsigned 256-bit integers. Several
//! operations (`a * b / d`) need a 512-bit intermediate to avoid overflow, so
//! we widen through [`U512`] and narrow back.

use crate::types::{U256, U512};

#[inline]
fn to_u512(x: U256) -> U512 {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(&x.to_le_bytes::<32>());
    U512::from_le_slice(&buf)
}

#[inline]
fn from_u512(x: U512) -> Option<U256> {
    let bytes = x.to_le_bytes::<64>();
    if bytes[32..].iter().any(|&b| b != 0) {
        return None; // does not fit in 256 bits
    }
    Some(U256::from_le_slice(&bytes[..32]))
}

/// `floor(a * b / denom)` computed with a 512-bit intermediate.
/// Returns `None` on division by zero or if the result exceeds 256 bits.
#[inline]
pub fn mul_div(a: U256, b: U256, denom: U256) -> Option<U256> {
    if denom.is_zero() {
        return None;
    }
    let prod = to_u512(a) * to_u512(b);
    from_u512(prod / to_u512(denom))
}

/// `ceil(a * b / denom)` computed with a 512-bit intermediate.
#[inline]
pub fn mul_div_rounding_up(a: U256, b: U256, denom: U256) -> Option<U256> {
    if denom.is_zero() {
        return None;
    }
    let prod = to_u512(a) * to_u512(b);
    let d = to_u512(denom);
    let mut q = prod / d;
    if !(prod % d).is_zero() {
        q += U512::from(1u64);
    }
    from_u512(q)
}

/// Integer square root (floor) via Babylonian iteration.
#[inline]
pub fn sqrt(n: U256) -> U256 {
    if n.is_zero() {
        return U256::ZERO;
    }
    let mut x = n;
    let mut y = (x + U256::from(1u64)) >> 1usize;
    while y < x {
        x = y;
        y = (x + n / x) >> 1usize;
    }
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mul_div_basic() {
        let r = mul_div(U256::from(6u64), U256::from(7u64), U256::from(2u64)).unwrap();
        assert_eq!(r, U256::from(21u64));
    }

    #[test]
    fn mul_div_no_overflow() {
        // a*b overflows U256 (MAX*2) but the quotient (MAX) fits.
        let big = U256::MAX;
        let r = mul_div(big, U256::from(2u64), U256::from(2u64)).unwrap();
        assert_eq!(r, big);
        // And a result that genuinely exceeds 256 bits returns None.
        assert_eq!(mul_div(big, U256::from(4u64), U256::from(2u64)), None);
    }

    #[test]
    fn mul_div_rounds_up() {
        let r = mul_div_rounding_up(U256::from(7u64), U256::from(1u64), U256::from(2u64)).unwrap();
        assert_eq!(r, U256::from(4u64));
    }

    #[test]
    fn sqrt_floor() {
        assert_eq!(sqrt(U256::from(0u64)), U256::from(0u64));
        assert_eq!(sqrt(U256::from(15u64)), U256::from(3u64));
        assert_eq!(sqrt(U256::from(16u64)), U256::from(4u64));
        let big = U256::from(1_000_000_000_000u128);
        assert_eq!(sqrt(big * big), big);
    }
}
