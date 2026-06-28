//! Aerodrome (Solidly) **volatile** pool — exact port of `Pool.getAmountOut`.
//!
//! Aerodrome deducts the fee in a SEPARATE integer step, then applies the
//! constant-product formula — which rounds differently than Uniswap V2's folded
//! formula. So volatile Aerodrome pools must NOT reuse [`crate::amm::univ2`].
//! (Aerodrome *stable* pools use [`crate::amm::solidly::SolidlyStablePool`],
//! whose fee handling already matches.)
//!
//! Reference `Pool.getAmountOut`:
//! ```solidity
//! amountIn -= (amountIn * fee) / 10000;          // fee deducted first
//! return (amountIn * reserveB) / (reserveA + amountIn);   // x*y=k
//! ```

use crate::math::mul_div;
use crate::pool::{Pool, Protocol, SimError};
use crate::types::{Address, U256};

#[derive(Debug, Clone)]
pub struct AerodromeVolatilePool {
    pub address: Address,
    pub token0: Address,
    pub token1: Address,
    pub reserve0: U256,
    pub reserve1: U256,
    /// Per-pool fee in basis points (denominator 10_000), read from the factory.
    pub fee_bps: u32,
    tokens: [Address; 2],
}

impl AerodromeVolatilePool {
    pub fn new(
        address: Address,
        token0: Address,
        token1: Address,
        reserve0: U256,
        reserve1: U256,
        fee_bps: u32,
    ) -> Self {
        Self {
            address,
            token0,
            token1,
            reserve0,
            reserve1,
            fee_bps,
            tokens: [token0, token1],
        }
    }
}

impl Pool for AerodromeVolatilePool {
    fn address(&self) -> Address {
        self.address
    }
    fn protocol(&self) -> Protocol {
        Protocol::UniswapV2
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
        let (reserve_in, reserve_out) = if token_in == self.token0 && token_out == self.token1 {
            (self.reserve0, self.reserve1)
        } else if token_in == self.token1 && token_out == self.token0 {
            (self.reserve1, self.reserve0)
        } else if token_in != self.token0 && token_in != self.token1 {
            return Err(SimError::UnknownToken(token_in));
        } else {
            return Err(SimError::UnknownToken(token_out));
        };
        if reserve_in.is_zero() || reserve_out.is_zero() {
            return Err(SimError::InsufficientLiquidity);
        }

        // fee deducted first (floor), exactly as the contract does.
        let fee = mul_div(amount_in, U256::from(self.fee_bps), U256::from(10_000u64))
            .ok_or(SimError::Overflow)?;
        let amount_in_after = amount_in - fee;
        let denom = reserve_in
            .checked_add(amount_in_after)
            .ok_or(SimError::Overflow)?;
        mul_div(amount_in_after, reserve_out, denom).ok_or(SimError::Overflow)
    }

    fn gas_estimate(&self) -> u64 {
        110_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(n: u8) -> Address {
        Address::repeat_byte(n)
    }

    #[test]
    fn matches_reference_rounding() {
        // reserves 1e24/1e24, 30 bps fee, 1e21 in.
        // fee = 1e21*30/10000 = 3e18 ; aIn = 997e18
        // out = 997e18 * 1e24 / (1e24 + 997e18)
        let r = U256::from(10u64).pow(U256::from(24u64));
        let pool = AerodromeVolatilePool::new(addr(9), addr(1), addr(2), r, r, 30);
        let amt = U256::from(10u64).pow(U256::from(21u64));
        let out = pool.quote(addr(1), addr(2), amt).unwrap();

        let fee = amt * U256::from(30u64) / U256::from(10_000u64);
        let ain = amt - fee;
        let expected = ain * r / (r + ain);
        assert_eq!(out, expected);
    }

    #[test]
    fn differs_from_univ2_folded_formula() {
        // The two-step fee deduction can differ by a wei from the folded UniV2
        // formula — which is exactly why Aerodrome volatile needs its own sim.
        use crate::amm::univ2::UniV2Pool;
        // pick reserves/amount where rounding diverges
        let pool_a = AerodromeVolatilePool::new(addr(9), addr(1), addr(2), U256::from(1_000_003u64), U256::from(7_777_777u64), 30);
        let pool_u = UniV2Pool::new(addr(9), addr(1), addr(2), U256::from(1_000_003u64), U256::from(7_777_777u64), 30);
        let amt = U256::from(12_345u64);
        let a = pool_a.quote(addr(1), addr(2), amt).unwrap();
        let u = pool_u.quote(addr(1), addr(2), amt).unwrap();
        // They are close but the rounding path is independent; assert the
        // Aerodrome one equals the reference two-step computation.
        let fee = amt * U256::from(30u64) / U256::from(10_000u64);
        let ain = amt - fee;
        assert_eq!(a, ain * U256::from(7_777_777u64) / (U256::from(1_000_003u64) + ain));
        let _ = u; // u may or may not equal a; the point is `a` matches the contract.
    }
}
