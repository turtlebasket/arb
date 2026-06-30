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

/// Saturating `U256 -> i128` (our loop values are USDC-scale, far under i128).
fn to_i128_sat(x: U256) -> i128 {
    let u = u128::try_from(x).unwrap_or(u128::MAX);
    if u > i128::MAX as u128 {
        i128::MAX
    } else {
        u as i128
    }
}

/// The size-optimal evaluation of a closed loop.
#[derive(Debug, Clone, Copy)]
pub struct SizedResult {
    /// Trade size (input-token units) that maximizes net profit.
    pub amount_in: U256,
    pub result: NetResult,
}

/// Find the trade size in `[min_amount, max_amount]` that maximizes net profit
/// for a closed loop, instead of probing one fixed size.
///
/// The arb profit curve `gross_out(x) - x` is concave with a single interior
/// maximum (marginal output decreases as the trade walks up the AMM curves), so
/// a **ternary search** converges on the optimum. Two guards keep it cheap and
/// correct:
///   1. **Marginal pre-filter** — if the loop's effective rate at infinitesimal
///      size is `<= 1` (i.e. `gross(2s) - gross(s) <= s` for a tiny `s`), the
///      concave curve never rises above break-even and there is *no* profitable
///      size; return `None` after just two sims. This is the mathematically
///      exact "does an arb exist" test and skips the search for the ~all cycles
///      that aren't arbs.
///   2. **Revert = -inf** — a leg that reverts (e.g. V3 windowed state declining
///      an oversized swap, or insufficient liquidity) scores `i128::MIN`, so the
///      search is naturally bounded to the feasible size range.
///
/// Returns the best size and its [`NetResult`] for any cycle whose marginal rate
/// exceeds 1 (a latent arb); the caller decides profitability via
/// `result.net_profit` against its own `min_profit`. `None` means no arb exists
/// (rate `<= 1`) or the loop reverts even at the marginal probe size.
pub fn best_size(
    legs: &[Leg<'_>],
    min_amount: U256,
    max_amount: U256,
    gas_price_wei: U256,
    native_per_token_q: U256,
) -> Option<SizedResult> {
    if min_amount.is_zero() || max_amount <= min_amount {
        return None;
    }
    // (1) Marginal pre-filter: effective rate > 1 at small size? Probe at a size
    // small enough to be ~marginal but large enough that the first hop doesn't
    // round to zero (which would falsely reject a real arb). Scale it off the cap
    // so it adapts to the token's unit size.
    let s = core::cmp::max(min_amount, max_amount / U256::from(10_000u64));
    let two_s = s.checked_mul(U256::from(2u64))?;
    if two_s >= max_amount {
        return None; // range too narrow to search meaningfully
    }
    let g1 = simulate_path(legs, s).ok()?;
    let g2 = simulate_path(legs, two_s).ok()?;
    // gross gained over the step must exceed the input added (rate > 1).
    if g2.checked_sub(g1)? <= (two_s - s) {
        return None; // rate <= 1 -> concave curve never clears break-even
    }

    let signed_net = |x: U256| -> i128 {
        match net_profit(legs, x, gas_price_wei, native_per_token_q) {
            Ok(r) => to_i128_sat(r.gross_out)
                .saturating_sub(to_i128_sat(x))
                .saturating_sub(to_i128_sat(r.gas_cost_in_token)),
            Err(_) => i128::MIN,
        }
    };

    // (2) Ternary search for the maximizing size. ~64 thirds-narrowings shrink
    // the bracket by 1.5^-64 — far below wei for any size we trade.
    let (mut lo, mut hi) = (min_amount, max_amount);
    let one = U256::from(1u64);
    for _ in 0..64 {
        if hi <= lo + one {
            break;
        }
        let third = (hi - lo) / U256::from(3u64);
        if third.is_zero() {
            break;
        }
        let m1 = lo + third;
        let m2 = hi - third;
        if signed_net(m1) < signed_net(m2) {
            lo = m1;
        } else {
            hi = m2;
        }
    }

    // Pick the best of the final bracket endpoints + midpoint.
    let mid = lo + (hi - lo) / U256::from(2u64);
    let mut best: Option<(U256, NetResult, i128)> = None;
    for x in [lo, mid, hi] {
        if let Ok(r) = net_profit(legs, x, gas_price_wei, native_per_token_q) {
            let sc = signed_net(x);
            if best.as_ref().is_none_or(|(_, _, bs)| sc > *bs) {
                best = Some((x, r, sc));
            }
        }
    }
    best.map(|(amount_in, result, _)| SizedResult { amount_in, result })
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

    #[test]
    fn best_size_finds_the_profit_peak() {
        let usdc = addr(1);
        let weth = addr(2);
        // Priced-apart pools => a real arb with a finite optimal size.
        let pool_a = UniV2Pool::new(addr(10), usdc, weth, U256::from(1_000_000u64), U256::from(1_100_000u64), 30);
        let pool_b = UniV2Pool::new(addr(11), usdc, weth, U256::from(1_000_000u64), U256::from(900_000u64), 30);
        let legs = vec![
            Leg { pool: &pool_a, token_in: usdc, token_out: weth },
            Leg { pool: &pool_b, token_in: weth, token_out: usdc },
        ];
        let sized = best_size(&legs, U256::from(1u64), U256::from(900_000u64), U256::ZERO, U256::from(1u64))
            .expect("an arb exists -> Some");
        let net = sized.result.net_profit.expect("optimal size is profitable");
        // The optimum is interior: better than a tiny probe and a near-max probe.
        let small = net_profit(&legs, U256::from(10u64), U256::ZERO, U256::from(1u64)).unwrap();
        let huge = net_profit(&legs, U256::from(800_000u64), U256::ZERO, U256::from(1u64)).unwrap();
        let prof = |o: Option<U256>| o.unwrap_or(U256::ZERO);
        assert!(net > prof(small.net_profit), "optimum beats a tiny size");
        assert!(net > prof(huge.net_profit), "optimum beats an oversized trade");
        assert!(sized.amount_in > U256::from(10u64) && sized.amount_in < U256::from(800_000u64));
    }

    #[test]
    fn best_size_none_when_no_arb() {
        let usdc = addr(1);
        let weth = addr(2);
        // Identical pools => closed loop only ever loses fees: rate <= 1.
        let pool_a = UniV2Pool::new(addr(10), usdc, weth, U256::from(1_000_000u64), U256::from(1_000_000u64), 30);
        let pool_b = UniV2Pool::new(addr(11), usdc, weth, U256::from(1_000_000u64), U256::from(1_000_000u64), 30);
        let legs = vec![
            Leg { pool: &pool_a, token_in: usdc, token_out: weth },
            Leg { pool: &pool_b, token_in: weth, token_out: usdc },
        ];
        assert!(best_size(&legs, U256::from(1u64), U256::from(900_000u64), U256::ZERO, U256::from(1u64)).is_none());
    }
}
