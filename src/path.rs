//! Multi-hop path simulation across heterogeneous pools.
//!
//! An arb path is a sequence of legs starting and ending in the same token
//! (USDC → ... → USDC). [`simulate_path`] chains the per-pool [`Pool::quote`]
//! calls, and [`net_profit`] converts the gross edge into a gas- and
//! fee-adjusted net result in input-token units.

use crate::types::{Address, U256};

use crate::pool::{Pool, SimError};

/// One hop: trade `token_in` for `token_out` through `pool`.
pub struct Leg<'a> {
    pub pool: &'a dyn Pool,
    pub token_in: Address,
    pub token_out: Address,
}

/// Run `amount_in` of the first leg's input token through every leg in order.
/// Returns the final output amount (still gross — no gas deducted).
pub fn simulate_path(legs: &[Leg<'_>], amount_in: U256) -> Result<U256, SimError> {
    if legs.is_empty() {
        return Err(SimError::ZeroAmount);
    }
    let mut amount = amount_in;
    for leg in legs {
        amount = leg.pool.quote(leg.token_in, leg.token_out, amount)?;
    }
    Ok(amount)
}

/// Total gas (units) to execute the whole path.
pub fn path_gas(legs: &[Leg<'_>]) -> u64 {
    legs.iter().map(|l| l.pool.gas_estimate()).sum()
}

/// Result of evaluating a closed loop path.
#[derive(Debug, Clone, Copy)]
pub struct NetResult {
    /// Output of the final leg, in the input token's units.
    pub gross_out: U256,
    /// Gas cost expressed in input-token units.
    pub gas_cost_in_token: U256,
    /// `gross_out - amount_in - gas_cost`, if positive.
    pub net_profit: Option<U256>,
}

/// Evaluate a closed loop (input token == output token) net of gas.
///
/// `gas_price_wei` is the chain's gas price, `native_per_token_q` is the price
/// of 1 unit of the input token denominated in the chain's native gas token,
/// scaled by `1e18` (i.e. `native_wei_per_1_token * 1e18 / token_unit`). For a
/// USDC loop this is "how many wei of native (ETH/BNB/TRX) does 1 USDC unit buy
/// × 1e18". Keeping it as a ratio avoids needing a price oracle here.
pub fn net_profit(
    legs: &[Leg<'_>],
    amount_in: U256,
    gas_price_wei: U256,
    token_units_per_native_1e18: U256,
) -> Result<NetResult, SimError> {
    let gross_out = simulate_path(legs, amount_in)?;
    let gas_units = U256::from(path_gas(legs));
    let gas_wei = gas_units
        .checked_mul(gas_price_wei)
        .ok_or(SimError::Overflow)?;
    // gas_cost_in_token = gas_wei * token_units_per_native_1e18 / 1e18
    let gas_cost_in_token = crate::math::mul_div(
        gas_wei,
        token_units_per_native_1e18,
        U256::from(10u64).pow(U256::from(18u64)),
    )
    .ok_or(SimError::Overflow)?;

    let net_profit = gross_out
        .checked_sub(amount_in)
        .and_then(|edge| edge.checked_sub(gas_cost_in_token));

    Ok(NetResult {
        gross_out,
        gas_cost_in_token,
        net_profit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amm::univ2::UniV2Pool;

    fn addr(n: u8) -> Address {
        Address::repeat_byte(n)
    }

    #[test]
    fn two_hop_loop_profitable_when_priced_apart() {
        let usdc = addr(1);
        let weth = addr(2);
        // Pool A: cheap WETH (lots of WETH per USDC). Pool B: expensive WETH.
        let pool_a = UniV2Pool::new(addr(10), usdc, weth, U256::from(1_000_000u64), U256::from(1_100_000u64), 30);
        let pool_b = UniV2Pool::new(addr(11), usdc, weth, U256::from(1_000_000u64), U256::from(900_000u64), 30);
        let legs = vec![
            Leg { pool: &pool_a, token_in: usdc, token_out: weth },
            Leg { pool: &pool_b, token_in: weth, token_out: usdc },
        ];
        let out = simulate_path(&legs, U256::from(1000u64)).unwrap();
        // buy weth cheap on A, sell dear on B => more usdc out than in.
        assert!(out > U256::from(1000u64), "expected profit, got {out}");
    }

    #[test]
    fn net_profit_subtracts_gas() {
        let usdc = addr(1);
        let weth = addr(2);
        let pool_a = UniV2Pool::new(addr(10), usdc, weth, U256::from(1_000_000u64), U256::from(1_100_000u64), 30);
        let pool_b = UniV2Pool::new(addr(11), usdc, weth, U256::from(1_000_000u64), U256::from(900_000u64), 30);
        let legs = vec![
            Leg { pool: &pool_a, token_in: usdc, token_out: weth },
            Leg { pool: &pool_b, token_in: weth, token_out: usdc },
        ];
        // Zero gas price => net == gross edge.
        let r = net_profit(&legs, U256::from(1000u64), U256::ZERO, U256::from(1u64)).unwrap();
        assert!(r.net_profit.is_some());
        assert_eq!(r.gas_cost_in_token, U256::ZERO);
    }
}
