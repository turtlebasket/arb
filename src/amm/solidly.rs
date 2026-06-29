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

    /// Solidity `(a * b) / 1e18` with a **256-bit** multiply: `None` (≡ revert) at
    /// exactly the overflow point the on-chain contract hits. This faithful
    /// overflow behavior is required for wei-exactness on degenerate pools — e.g.
    /// a "stable" pool pairing tokens of very different value, where the invariant
    /// `x3y+xy3` overflows uint256 and the real `getAmountOut` reverts. (Using a
    /// 512-bit `mul_div` here would return a phantom value the chain can't.)
    #[inline]
    fn mul1e18(a: U256, b: U256) -> Option<U256> {
        Some(a.checked_mul(b)? / e18())
    }

    /// `_k`/`_f`: `x3y + xy3` on values normalized to 1e18, in the contract's
    /// exact expression order: `a=(x*y)/1e18`, `b=(x*x)/1e18+(y*y)/1e18`,
    /// `(a*b)/1e18`. (`_k` on raw reserves and `_f` on a candidate `y` are the
    /// same expression on-chain.)
    fn f(x: U256, y: U256) -> Option<U256> {
        let a = Self::mul1e18(x, y)?;
        let b = Self::mul1e18(x, x)?.checked_add(Self::mul1e18(y, y)?)?;
        Self::mul1e18(a, b)
    }

    /// `_d = (3*x0*((y*y)/1e18))/1e18 + ((((x0*x0)/1e18)*x0)/1e18)`.
    fn d(x0: U256, y: U256) -> Option<U256> {
        let y2 = Self::mul1e18(y, y)?;
        let t1 = U256::from(3u64).checked_mul(x0)?.checked_mul(y2)? / e18();
        let x2 = Self::mul1e18(x0, x0)?;
        let t2 = x2.checked_mul(x0)? / e18();
        t1.checked_add(t2)
    }

    /// `_k` verbatim — re-scales BOTH args by `decimals0`/`decimals1` (the
    /// contract does this even when called from `_get_y` with already-normalized
    /// args; a quirk we must replicate for wei-exactness).
    fn k_rescaled(&self, x: U256, y: U256) -> Option<U256> {
        let _x = x.checked_mul(e18())? / pow10(self.decimals0);
        let _y = y.checked_mul(e18())? / pow10(self.decimals1);
        Self::f(_x, _y)
    }

    /// Verbatim port of the contract's `_get_y` Newton solve, including the exact
    /// `dy == 0` handling (forces `dy = 1`, can oscillate) and the
    /// **`revert("!y")` after 255 iterations** if it never converges. All
    /// arithmetic is 256-bit checked so overflow/underflow/zero-division yield
    /// `None` (≡ revert). On ill-conditioned pools (e.g. tokens of very different
    /// value mis-paired as "stable") the integer root can't satisfy the return
    /// conditions, so it oscillates to 255 and reverts — exactly like on-chain.
    fn get_y(&self, x0: U256, xy: U256, mut y: U256) -> Option<U256> {
        let one = U256::from(1u64);
        for _ in 0..255 {
            let k = Self::f(x0, y)?;
            let dd = Self::d(x0, y)?;
            if dd.is_zero() {
                return None; // div by _d -> revert
            }
            if k < xy {
                let dy = (xy - k).checked_mul(e18())? / dd;
                if dy.is_zero() {
                    if k == xy {
                        return Some(y);
                    }
                    if self.k_rescaled(x0, y.checked_add(one)?)? > xy {
                        return Some(y + one);
                    }
                    y = y.checked_add(one)?; // dy = 1
                } else {
                    y = y.checked_add(dy)?;
                }
            } else {
                let dy = (k - xy).checked_mul(e18())? / dd;
                if dy.is_zero() {
                    if k == xy || Self::f(x0, y.checked_sub(one)?)? < xy {
                        return Some(y);
                    }
                    y = y.checked_sub(one)?; // dy = 1
                } else {
                    y = y.checked_sub(dy)?; // underflow -> None ≡ revert
                }
            }
        }
        None // contract: revert("!y") on non-convergence
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
        let from_token0 = if token_in == self.token0 && token_out == self.token1 {
            true
        } else if token_in == self.token1 && token_out == self.token0 {
            false
        } else if token_in != self.token0 && token_in != self.token1 {
            return Err(SimError::UnknownToken(token_in));
        } else {
            return Err(SimError::UnknownToken(token_out));
        };
        if self.reserve0.is_zero() || self.reserve1.is_zero() {
            return Err(SimError::InsufficientLiquidity);
        }
        let d0 = pow10(self.decimals0); // contract's `decimals0` = 10**dec
        let d1 = pow10(self.decimals1);

        // amountIn -= (amountIn * fee) / 10000   (matches `_getAmountOut`)
        let fee = mul_div(amount_in, U256::from(self.fee_bps), U256::from(10_000u64))
            .ok_or(SimError::Overflow)?;
        let amount_in = amount_in - fee;

        // xy = _k(reserve0, reserve1) on RAW reserves, scaled to 1e18 internally
        // (token order, not in/out order — the invariant is symmetric).
        let rx = self.reserve0.checked_mul(e18()).ok_or(SimError::Overflow)? / d0;
        let ry = self.reserve1.checked_mul(e18()).ok_or(SimError::Overflow)? / d1;
        let xy = Self::f(rx, ry).ok_or(SimError::Overflow)?;

        let (reserve_a, reserve_b) = if from_token0 { (rx, ry) } else { (ry, rx) };
        let amt_in = if from_token0 {
            amount_in.checked_mul(e18()).ok_or(SimError::Overflow)? / d0
        } else {
            amount_in.checked_mul(e18()).ok_or(SimError::Overflow)? / d1
        };
        let x0 = amt_in.checked_add(reserve_a).ok_or(SimError::Overflow)?;
        // get_y → None means the contract reverts (overflow/underflow/non-convergence).
        let new_y = self.get_y(x0, xy, reserve_b).ok_or(SimError::Overflow)?;
        let y = reserve_b.checked_sub(new_y).ok_or(SimError::InsufficientLiquidity)?;

        // out = (y * decimals_out) / 1e18
        let dec_out_factor = if from_token0 { d1 } else { d0 };
        Ok(y.checked_mul(dec_out_factor).ok_or(SimError::Overflow)? / e18())
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

    #[test]
    fn degenerate_weth_usdc_stable_reverts_like_chain() {
        // Real Base pool 0x3548… (WETH/USDC mis-paired as "stable") at block
        // 47978632: reserves 4.354 WETH / 20434 USDC. On-chain getAmountOut for
        // 0.01 WETH reverts (panic 0x11 overflow) — the Newton solve is
        // ill-conditioned. The sim MUST also fail (return Err), not a phantom value.
        let weth = addr(1);
        let usdc = addr(2);
        let r0 = U256::from_str_radix("3c6f3bb56ce6567f", 16).unwrap(); // WETH (18)
        let r1 = U256::from_str_radix("4c1e205bb", 16).unwrap(); // USDC (6)
        let pool = SolidlyStablePool::new(addr(9), weth, usdc, r0, r1, 18, 6, 5);
        let out = pool.quote(weth, usdc, U256::from(10_000_000_000_000_000u64)); // 0.01 WETH
        assert!(out.is_err(), "expected revert-equivalent Err, got {out:?}");
    }
}
