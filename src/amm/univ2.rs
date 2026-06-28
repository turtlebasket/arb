//! Constant-product (`x * y = k`) pools.
//!
//! Covers Uniswap V2 and its forks: PancakeSwap V2 (25 bps), SunSwap V2,
//! and Solidly *volatile* pools — they only differ by the fee parameter.

use crate::types::{Address, U256};

use crate::math::mul_div;
use crate::pool::{Pool, Protocol, SimError};

/// A constant-product pair with a configurable fee.
#[derive(Debug, Clone)]
pub struct UniV2Pool {
    pub address: Address,
    pub token0: Address,
    pub token1: Address,
    pub reserve0: U256,
    pub reserve1: U256,
    /// Swap fee in basis points (1 bp = 0.01%). Uniswap V2 = 30, Pancake = 25.
    pub fee_bps: u32,
    tokens: [Address; 2],
}

impl UniV2Pool {
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

    /// `(reserve_in, reserve_out)` oriented for the given input token.
    fn reserves_for(&self, token_in: Address) -> Option<(U256, U256)> {
        if token_in == self.token0 {
            Some((self.reserve0, self.reserve1))
        } else if token_in == self.token1 {
            Some((self.reserve1, self.reserve0))
        } else {
            None
        }
    }
}

impl Pool for UniV2Pool {
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
        let (reserve_in, reserve_out) = self
            .reserves_for(token_in)
            .ok_or(SimError::UnknownToken(token_in))?;
        if token_out != self.token0 && token_out != self.token1 {
            return Err(SimError::UnknownToken(token_out));
        }
        if reserve_in.is_zero() || reserve_out.is_zero() {
            return Err(SimError::InsufficientLiquidity);
        }

        // amount_out = (amount_in * (1-fee) * reserve_out)
        //              / (reserve_in + amount_in * (1-fee))
        let fee_num = U256::from(10_000 - self.fee_bps);
        let amount_in_with_fee = amount_in
            .checked_mul(fee_num)
            .ok_or(SimError::Overflow)?;
        let denominator = reserve_in
            .checked_mul(U256::from(10_000u64))
            .ok_or(SimError::Overflow)?
            .checked_add(amount_in_with_fee)
            .ok_or(SimError::Overflow)?;
        // 512-bit intermediate: amount_in_with_fee * reserve_out may exceed 256 bits.
        mul_div(amount_in_with_fee, reserve_out, denominator).ok_or(SimError::Overflow)
    }

    fn gas_estimate(&self) -> u64 {
        // A single V2 swap incl. transfer + sync, observed ~90–110k.
        100_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(n: u8) -> Address {
        Address::repeat_byte(n)
    }

    #[test]
    fn official_get_amount_out_vectors() {
        // From Uniswap v2-periphery UniswapV2Library getAmountOut tests
        // (0.30% fee). getAmountOut(2, 100, 100) == 1.
        let pool = UniV2Pool::new(
            addr(9),
            addr(1),
            addr(2),
            U256::from(100u64),
            U256::from(100u64),
            30,
        );
        assert_eq!(
            pool.quote(addr(1), addr(2), U256::from(2u64)).unwrap(),
            U256::from(1u64)
        );

        // getAmountOut(10, 1000, 1000): 10*997*1000/(1000*1000+10*997) = 9.
        let pool = UniV2Pool::new(
            addr(9),
            addr(1),
            addr(2),
            U256::from(1000u64),
            U256::from(1000u64),
            30,
        );
        assert_eq!(
            pool.quote(addr(1), addr(2), U256::from(10u64)).unwrap(),
            U256::from(9u64)
        );
    }

    #[test]
    fn matches_uniswap_v2_formula() {
        // reserves 1000 / 1000, 0.3% fee, 100 in.
        // out = 1000*997*100 / (1000*1000 + 997*100) = 99700000 / 1099700 = 90.66..
        let pool = UniV2Pool::new(
            addr(9),
            addr(1),
            addr(2),
            U256::from(1000u64),
            U256::from(1000u64),
            30,
        );
        let out = pool
            .quote(addr(1), addr(2), U256::from(100u64))
            .unwrap();
        assert_eq!(out, U256::from(90u64));
    }

    #[test]
    fn symmetric_directions() {
        let pool = UniV2Pool::new(
            addr(9),
            addr(1),
            addr(2),
            U256::from(5_000_000u64),
            U256::from(5_000_000u64),
            30,
        );
        let a = pool.quote(addr(1), addr(2), U256::from(10_000u64)).unwrap();
        let b = pool.quote(addr(2), addr(1), U256::from(10_000u64)).unwrap();
        assert_eq!(a, b); // symmetric reserves => symmetric quote
    }

    #[test]
    fn unknown_token_errors() {
        let pool = UniV2Pool::new(
            addr(9),
            addr(1),
            addr(2),
            U256::from(1000u64),
            U256::from(1000u64),
            30,
        );
        assert_eq!(
            pool.quote(addr(3), addr(2), U256::from(1u64)),
            Err(SimError::UnknownToken(addr(3)))
        );
    }
}
