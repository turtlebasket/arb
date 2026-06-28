//! Solidly *stable* pools (Aerodrome / Velodrome / Thena stable pairs).
//!
//! The invariant is `x^3*y + x*y^3 = k` evaluated on balances normalized to
//! 18 decimals. There is no closed form for the output, so the reference
//! implementation (and this port) solve for the new reserve with Newton's
//! method. Volatile Solidly pools are just constant-product — use
//! [`crate::amm::univ2::UniV2Pool`] for those.

use crate::types::{Address, U256};

use crate::math::mul_div;
use crate::pool::{Pool, Protocol, SimError};

#[inline]
fn e18() -> U256 {
    U256::from(1_000_000_000_000_000_000u64)
}

#[inline]
fn pow10(dec: u8) -> U256 {
    U256::from(10u64).pow(U256::from(dec as u64))
}

#[derive(Debug, Clone)]
pub struct SolidlyStablePool {
    pub address: Address,
    pub token0: Address,
    pub token1: Address,
    /// Raw reserves in native token units (not normalized).
    pub reserve0: U256,
    pub reserve1: U256,
    pub decimals0: u8,
    pub decimals1: u8,
    /// Fee in basis points (Aerodrome stable pools are typically 5 bps).
    pub fee_bps: u32,
    tokens: [Address; 2],
}

impl SolidlyStablePool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        address: Address,
        token0: Address,
        token1: Address,
        reserve0: U256,
        reserve1: U256,
        decimals0: u8,
        decimals1: u8,
        fee_bps: u32,
    ) -> Self {
        Self {
            address,
            token0,
            token1,
            reserve0,
            reserve1,
            decimals0,
            decimals1,
            fee_bps,
            tokens: [token0, token1],
        }
    }

    /// Normalize a raw amount of `dec`-decimal token to 1e18 fixed point.
    fn norm(amount: U256, dec: u8) -> Option<U256> {
        mul_div(amount, e18(), pow10(dec))
    }

    /// Denormalize a 1e18 fixed-point amount back to `dec`-decimal units.
    fn denorm(amount: U256, dec: u8) -> Option<U256> {
        mul_div(amount, pow10(dec), e18())
    }

    /// k = x^3*y + x*y^3 with x,y normalized to 1e18.
    fn k(x: U256, y: U256) -> Option<U256> {
        let a = mul_div(x, y, e18())?; // xy / 1e18
        let x2 = mul_div(x, x, e18())?;
        let y2 = mul_div(y, y, e18())?;
        let b = x2.checked_add(y2)?; // (x^2 + y^2)/1e18
        mul_div(a, b, e18())
    }

    /// f(x0, y) = x0*y^3 + x0^3*y  (the invariant value at a candidate y).
    fn f(x0: U256, y: U256) -> Option<U256> {
        let y2 = mul_div(y, y, e18())?;
        let y3 = mul_div(y2, y, e18())?;
        let t1 = mul_div(x0, y3, e18())?;
        let x2 = mul_div(x0, x0, e18())?;
        let x3 = mul_div(x2, x0, e18())?;
        let t2 = mul_div(x3, y, e18())?;
        t1.checked_add(t2)
    }

    /// d/dy f = 3*x0*y^2 + x0^3.
    fn d(x0: U256, y: U256) -> Option<U256> {
        let y2 = mul_div(y, y, e18())?;
        let three_x0 = x0.checked_mul(U256::from(3u64))?;
        let t1 = mul_div(three_x0, y2, e18())?;
        let x2 = mul_div(x0, x0, e18())?;
        let x3 = mul_div(x2, x0, e18())?;
        t1.checked_add(x3)
    }

    /// Solve for the new reserve `y` given `x0` (new reserve in) and target `xy`.
    fn get_y(x0: U256, xy: U256, mut y: U256) -> Option<U256> {
        let one = U256::from(1u64);
        for _ in 0..255 {
            let y_prev = y;
            let k = Self::f(x0, y)?;
            let dydx = Self::d(x0, y)?;
            if dydx.is_zero() {
                return None;
            }
            if k < xy {
                let dy = mul_div(xy - k, e18(), dydx)?;
                y = y.checked_add(dy)?;
            } else {
                let dy = mul_div(k - xy, e18(), dydx)?;
                y = y.checked_sub(dy)?;
            }
            if y > y_prev {
                if y - y_prev <= one {
                    return Some(y);
                }
            } else if y_prev - y <= one {
                return Some(y);
            }
        }
        Some(y)
    }
}

impl Pool for SolidlyStablePool {
    fn address(&self) -> Address {
        self.address
    }

    fn protocol(&self) -> Protocol {
        Protocol::SolidlyStable
    }

    fn tokens(&self) -> &[Address] {
        &self.tokens
    }

    fn quote(
        &self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Result<U256, SimError> {
        if token_in == token_out {
            return Err(SimError::SameToken);
        }
        if amount_in.is_zero() {
            return Err(SimError::ZeroAmount);
        }
        if self.fee_bps >= 10_000 {
            return Err(SimError::BadConfig("fee_bps >= 100%"));
        }
        let (dec_in, dec_out, raw_res_in, raw_res_out) = if token_in == self.token0
            && token_out == self.token1
        {
            (self.decimals0, self.decimals1, self.reserve0, self.reserve1)
        } else if token_in == self.token1 && token_out == self.token0 {
            (self.decimals1, self.decimals0, self.reserve1, self.reserve0)
        } else if token_in != self.token0 && token_in != self.token1 {
            return Err(SimError::UnknownToken(token_in));
        } else {
            return Err(SimError::UnknownToken(token_out));
        };
        if raw_res_in.is_zero() || raw_res_out.is_zero() {
            return Err(SimError::InsufficientLiquidity);
        }

        // Apply fee on the input, then work in normalized (1e18) space.
        let fee = mul_div(amount_in, U256::from(self.fee_bps), U256::from(10_000u64))
            .ok_or(SimError::Overflow)?;
        let amount_in_after_fee = amount_in - fee;

        let res_in = Self::norm(raw_res_in, dec_in).ok_or(SimError::Overflow)?;
        let res_out = Self::norm(raw_res_out, dec_out).ok_or(SimError::Overflow)?;
        let amt_in = Self::norm(amount_in_after_fee, dec_in).ok_or(SimError::Overflow)?;

        let xy = Self::k(res_in, res_out).ok_or(SimError::Overflow)?;
        let new_res_in = res_in.checked_add(amt_in).ok_or(SimError::Overflow)?;
        let new_res_out = Self::get_y(new_res_in, xy, res_out).ok_or(SimError::Overflow)?;
        let dy = res_out
            .checked_sub(new_res_out)
            .ok_or(SimError::InsufficientLiquidity)?;

        Self::denorm(dy, dec_out).ok_or(SimError::Overflow)
    }

    fn gas_estimate(&self) -> u64 {
        // Newton iteration is done on-chain in view; swap itself ~110–130k.
        120_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(n: u8) -> Address {
        Address::repeat_byte(n)
    }

    #[test]
    fn stable_swap_near_peg() {
        // Two 18-decimal stables, deep balanced reserves. A small trade on the
        // stable curve should return very close to 1:1 (better than V2 would).
        let one_m = U256::from(1_000_000u64) * e18();
        let pool = SolidlyStablePool::new(
            addr(9),
            addr(1),
            addr(2),
            one_m,
            one_m,
            18,
            18,
            5,
        );
        let amount_in = U256::from(1000u64) * e18();
        let out = pool.quote(addr(1), addr(2), amount_in).unwrap();

        // Out should be slightly less than in (fee + slippage), but within ~0.1%.
        let lower = mul_div(amount_in, U256::from(999u64), U256::from(1000u64)).unwrap();
        assert!(out < amount_in, "out {out} should be < in {amount_in}");
        assert!(out > lower, "out {out} should be within 0.1% of in {amount_in}");
    }

    #[test]
    fn handles_mixed_decimals() {
        // token0 6-dec (USDC-like), token1 18-dec (DAI-like), both ~1M.
        let pool = SolidlyStablePool::new(
            addr(9),
            addr(1),
            addr(2),
            U256::from(1_000_000_000_000u64),      // 1M * 1e6
            U256::from(1_000_000u64) * e18(),       // 1M * 1e18
            6,
            18,
            5,
        );
        let amount_in = U256::from(1_000_000_000u64); // 1000 USDC (1e6)
        let out = pool.quote(addr(1), addr(2), amount_in).unwrap();
        // ~1000 DAI (1e18) expected, within ~0.2%.
        let expected = U256::from(1000u64) * e18();
        let lo = mul_div(expected, U256::from(998u64), U256::from(1000u64)).unwrap();
        let hi = mul_div(expected, U256::from(1002u64), U256::from(1000u64)).unwrap();
        assert!(out > lo && out < hi, "out={out} expected≈{expected}");
    }
}
