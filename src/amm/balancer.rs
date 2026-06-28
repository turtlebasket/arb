//! Balancer V2 weighted pools — EXACT integer math.
//!
//! Swap quoting mirrors `BaseWeightedPool._onSwapGivenIn` + the Vault's
//! up/down-scaling, delegating the core formula to [`balancer_math`] (a verbatim
//! port of `WeightedMath`/`LogExpMath`/`FixedPoint`). There are no floats: all
//! arithmetic is `U256`/`I256`.
//!
//! Flow for an exact-in swap:
//!   1. subtract the swap fee from the raw input (`amountIn.mulUp(swapFee)`),
//!   2. upscale balances + input to 18 decimals (`× 10^(18-decimals)`),
//!   3. `WeightedMath.calcOutGivenIn`,
//!   4. downscale the result back to the output token's decimals (truncating).

use crate::amm::balancer_math::{self as bmath, MathError};
use crate::types::{Address, U256};

use crate::pool::{Pool, Protocol, SimError};

fn map_err(e: MathError) -> SimError {
    match e {
        MathError::MaxInRatio => SimError::InsufficientLiquidity,
        _ => SimError::Overflow,
    }
}

#[derive(Debug, Clone)]
pub struct BalancerWeightedPool {
    pub address: Address,
    pub tokens: Vec<Address>,
    /// Balances in native token units, token order.
    pub balances: Vec<U256>,
    /// Normalized weights as 18-decimal fixed point (must sum to 1e18).
    pub weights: Vec<U256>,
    /// Token decimals (for up/down-scaling to 18-decimal math space).
    pub decimals: Vec<u8>,
    /// Swap fee as 18-decimal fixed point (e.g. 0.001e18 == 0.1%).
    pub swap_fee: U256,
}

impl BalancerWeightedPool {
    pub fn new(
        address: Address,
        tokens: Vec<Address>,
        balances: Vec<U256>,
        weights: Vec<U256>,
        decimals: Vec<u8>,
        swap_fee: U256,
    ) -> Self {
        Self {
            address,
            tokens,
            balances,
            weights,
            decimals,
            swap_fee,
        }
    }

    /// Convenience constructor taking the fee in basis points instead of the
    /// 1e18 fixed-point fraction (1 bp = 0.01% = 1e14 in fixed point).
    #[allow(clippy::too_many_arguments)]
    pub fn from_bps(
        address: Address,
        tokens: Vec<Address>,
        balances: Vec<U256>,
        weights: Vec<U256>,
        decimals: Vec<u8>,
        fee_bps: u32,
    ) -> Self {
        let swap_fee = U256::from(fee_bps) * U256::from(100_000_000_000_000u64); // bps * 1e14
        Self::new(address, tokens, balances, weights, decimals, swap_fee)
    }

    fn index_of(&self, token: Address) -> Option<usize> {
        self.tokens.iter().position(|t| *t == token)
    }

    /// Scaling factor `10^(18 - decimals)` for up/down-scaling.
    fn scale(dec: u8) -> U256 {
        U256::from(10u64).pow(U256::from(18u64 - dec as u64))
    }
}

impl Pool for BalancerWeightedPool {
    fn address(&self) -> Address {
        self.address
    }

    fn protocol(&self) -> Protocol {
        Protocol::BalancerWeighted
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
        let n = self.tokens.len();
        if self.balances.len() != n || self.weights.len() != n || self.decimals.len() != n {
            return Err(SimError::BadConfig("tokens/balances/weights/decimals mismatch"));
        }
        let i = self
            .index_of(token_in)
            .ok_or(SimError::UnknownToken(token_in))?;
        let j = self
            .index_of(token_out)
            .ok_or(SimError::UnknownToken(token_out))?;
        if self.decimals[i] > 18 || self.decimals[j] > 18 {
            return Err(SimError::BadConfig("token decimals > 18 unsupported"));
        }

        // 1. swap fee on raw input: amountIn - amountIn.mulUp(swapFee)
        let fee = bmath::mul_up(amount_in, self.swap_fee).map_err(map_err)?;
        let amount_in_after_fee = bmath::sub(amount_in, fee).map_err(map_err)?;

        // 2. upscale to 18 decimals
        let scale_in = Self::scale(self.decimals[i]);
        let scale_out = Self::scale(self.decimals[j]);
        let up_amount_in = amount_in_after_fee
            .checked_mul(scale_in)
            .ok_or(SimError::Overflow)?;
        let up_balance_in = self.balances[i]
            .checked_mul(scale_in)
            .ok_or(SimError::Overflow)?;
        let up_balance_out = self.balances[j]
            .checked_mul(scale_out)
            .ok_or(SimError::Overflow)?;
        if up_balance_in.is_zero() || up_balance_out.is_zero() {
            return Err(SimError::InsufficientLiquidity);
        }

        // 3. core weighted math
        let up_out = bmath::calc_out_given_in(
            up_balance_in,
            self.weights[i],
            up_balance_out,
            self.weights[j],
            up_amount_in,
        )
        .map_err(map_err)?;

        // 4. downscale (truncating) back to output token decimals
        Ok(up_out / scale_out)
    }

    fn gas_estimate(&self) -> u64 {
        140_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(n: u8) -> Address {
        Address::repeat_byte(n)
    }
    fn e18() -> U256 {
        U256::from(10u64).pow(U256::from(18u64))
    }
    fn half() -> U256 {
        U256::from(500_000_000_000_000_000u64)
    }

    #[test]
    fn fifty_fifty_18dec_matches_weighted_math() {
        // No fee, 18-decimal tokens, balances 100/100, in 10 => 9.0909e18 exactly
        // (the same value asserted in balancer_math's official-vector test).
        let pool = BalancerWeightedPool::new(
            addr(9),
            vec![addr(1), addr(2)],
            vec![U256::from(100u64) * e18(), U256::from(100u64) * e18()],
            vec![half(), half()],
            vec![18, 18],
            U256::ZERO,
        );
        let out = pool
            .quote(addr(1), addr(2), U256::from(10u64) * e18())
            .unwrap();
        assert_eq!(
            out,
            U256::from_str_radix("9090909090909090900", 10).unwrap()
        );
    }

    #[test]
    fn applies_swap_fee() {
        // With a 1% fee the output must be strictly less than the zero-fee case.
        let mk = |fee_bps: u32| {
            BalancerWeightedPool::from_bps(
                addr(9),
                vec![addr(1), addr(2)],
                vec![U256::from(100u64) * e18(), U256::from(100u64) * e18()],
                vec![half(), half()],
                vec![18, 18],
                fee_bps,
            )
        };
        let no_fee = mk(0).quote(addr(1), addr(2), U256::from(10u64) * e18()).unwrap();
        let with_fee = mk(100).quote(addr(1), addr(2), U256::from(10u64) * e18()).unwrap();
        assert!(with_fee < no_fee, "fee should reduce output: {with_fee} vs {no_fee}");
    }

    #[test]
    fn handles_mixed_decimals() {
        // token0 6-dec (USDC-like), token1 18-dec, 80/20 weights. Just sanity:
        // a positive output and no panic across the scaling boundary.
        let pool = BalancerWeightedPool::new(
            addr(9),
            vec![addr(1), addr(2)],
            vec![
                U256::from(2_000_000u64) * U256::from(10u64).pow(U256::from(6u64)),
                U256::from(8000u64) * e18(),
            ],
            vec![
                U256::from(200_000_000_000_000_000u64), // 0.2e18
                U256::from(800_000_000_000_000_000u64), // 0.8e18
            ],
            vec![6, 18],
            U256::ZERO,
        );
        let amount_in = U256::from(1000u64) * U256::from(10u64).pow(U256::from(6u64)); // 1000 USDC
        let out = pool.quote(addr(1), addr(2), amount_in).unwrap();
        assert!(out > U256::ZERO);
    }

    #[test]
    fn rejects_oversized_input() {
        let pool = BalancerWeightedPool::new(
            addr(9),
            vec![addr(1), addr(2)],
            vec![U256::from(100u64) * e18(), U256::from(100u64) * e18()],
            vec![half(), half()],
            vec![18, 18],
            U256::ZERO,
        );
        // > 30% of balance_in -> MAX_IN_RATIO -> InsufficientLiquidity
        let res = pool.quote(addr(1), addr(2), U256::from(40u64) * e18());
        assert_eq!(res, Err(SimError::InsufficientLiquidity));
    }
}
