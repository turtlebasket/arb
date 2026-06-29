//! Uniswap V3 concentrated-liquidity pools (also Pancake V3, Aerodrome
//! Slipstream — same math, different fee/tick-spacing).
//!
//! This is a direct port of the v3-core libraries `TickMath`,
//! `SqrtPriceMath` and `SwapMath`, plus the `Pool.swap` tick-crossing loop.
//! Given the pool's current price/liquidity and the set of initialized ticks
//! (each with its `liquidityNet`), it reproduces the exact output of an
//! exact-input swap.

use crate::types::{Address, U256};

use crate::math::{mul_div, mul_div_rounding_up};
use crate::pool::{Pool, Protocol, SimError};

pub const MIN_TICK: i32 = -887272;
pub const MAX_TICK: i32 = 887272;

fn min_sqrt_ratio() -> U256 {
    U256::from(4295128739u64)
}
fn max_sqrt_ratio() -> U256 {
    U256::from_str_radix("1461446703485210103287273052203988822378723970342", 10).unwrap()
}
fn q96() -> U256 {
    U256::from(1u64) << 96usize
}

/// An initialized tick and its net liquidity change when crossed upward.
#[derive(Debug, Clone, Copy)]
pub struct TickData {
    pub tick: i32,
    pub liquidity_net: i128,
}

#[derive(Debug, Clone)]
pub struct UniV3Pool {
    pub address: Address,
    pub token0: Address,
    pub token1: Address,
    pub fee_pips: u32, // fee in hundredths of a bip (1e-6). 500 = 0.05%.
    pub sqrt_price_x96: U256,
    pub liquidity: u128,
    pub tick: i32,
    /// Pool tick spacing (needed to replicate word-bounded tick stepping).
    pub tick_spacing: i32,
    /// Initialized ticks, sorted ascending by `tick`.
    pub ticks: Vec<TickData>,
    /// Tick range `[min, max]` for which `ticks` is COMPLETE (windowed state).
    /// `None` = full-range data. A swap that would move price beyond this window
    /// returns [`SimError::IncompleteState`] instead of a wrong value.
    pub known_range: Option<(i32, i32)>,
    tokens: [Address; 2],
}

impl UniV3Pool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        address: Address,
        token0: Address,
        token1: Address,
        fee_pips: u32,
        sqrt_price_x96: U256,
        liquidity: u128,
        tick: i32,
        tick_spacing: i32,
        mut ticks: Vec<TickData>,
    ) -> Self {
        ticks.sort_by_key(|t| t.tick);
        Self {
            address,
            token0,
            token1,
            fee_pips,
            sqrt_price_x96,
            liquidity,
            tick,
            tick_spacing: tick_spacing.max(1),
            ticks,
            known_range: None,
            tokens: [token0, token1],
        }
    }

    /// Mark the tick data as complete only within `[min, max]` (windowed state).
    pub fn with_known_range(mut self, min: i32, max: i32) -> Self {
        self.known_range = Some((min, max));
        self
    }

    /// TickMath.getSqrtRatioAtTick — Q64.96 sqrt price for a tick.
    pub fn get_sqrt_ratio_at_tick(tick: i32) -> Result<U256, SimError> {
        let abs_tick = tick.unsigned_abs() as u64;
        if abs_tick > MAX_TICK as u64 {
            return Err(SimError::BadConfig("tick out of range"));
        }
        // Each constant is a Q128.128 fixed-point factor.
        let consts: [(u64, &str); 20] = [
            (0x1, "fffcb933bd6fad37aa2d162d1a594001"),
            (0x2, "fff97272373d413259a46990580e213a"),
            (0x4, "fff2e50f5f656932ef12357cf3c7fdcc"),
            (0x8, "ffe5caca7e10e4e61c3624eaa0941cd0"),
            (0x10, "ffcb9843d60f6159c9db58835c926644"),
            (0x20, "ff973b41fa98c081472e6896dfb254c0"),
            (0x40, "ff2ea16466c96a3843ec78b326b52861"),
            (0x80, "fe5dee046a99a2a811c461f1969c3053"),
            (0x100, "fcbe86c7900a88aedcffc83b479aa3a4"),
            (0x200, "f987a7253ac413176f2b074cf7815e54"),
            (0x400, "f3392b0822b70005940c7a398e4b70f3"),
            (0x800, "e7159475a2c29b7443b29c7fa6e889d9"),
            (0x1000, "d097f3bdfd2022b8845ad8f792aa5825"),
            (0x2000, "a9f746462d870fdf8a65dc1f90e061e5"),
            (0x4000, "70d869a156d2a1b890bb3df62baf32f7"),
            (0x8000, "31be135f97d08fd981231505542fcfa6"),
            (0x10000, "9aa508b5b7a84e1c677de54f3e99bc9"),
            (0x20000, "5d6af8dedb81196699c329225ee604"),
            (0x40000, "2216e584f5fa1ea926041bedfe98"),
            (0x80000, "48a170391f7dc42444e8fa2"),
        ];

        let mut ratio: U256 = if abs_tick & 0x1 != 0 {
            U256::from_str_radix(consts[0].1, 16).unwrap()
        } else {
            U256::from(1u64) << 128usize
        };
        for &(bit, c) in consts.iter().skip(1) {
            if abs_tick & bit != 0 {
                let factor = U256::from_str_radix(c, 16).unwrap();
                ratio = (ratio * factor) >> 128usize;
            }
        }

        if tick > 0 {
            ratio = U256::MAX / ratio;
        }

        // Round up to Q64.96: (ratio >> 32) + (ratio % (1<<32) == 0 ? 0 : 1)
        let shifted = ratio >> 32usize;
        let rem = ratio & ((U256::from(1u64) << 32usize) - U256::from(1u64));
        Ok(if rem.is_zero() {
            shifted
        } else {
            shifted + U256::from(1u64)
        })
    }

    /// SqrtPriceMath.getAmount0Delta (unsigned, with rounding flag).
    fn get_amount0_delta(
        mut sqrt_a: U256,
        mut sqrt_b: U256,
        liquidity: u128,
        round_up: bool,
    ) -> Result<U256, SimError> {
        if sqrt_a > sqrt_b {
            std::mem::swap(&mut sqrt_a, &mut sqrt_b);
        }
        let numerator1 = U256::from(liquidity) << 96usize;
        let numerator2 = sqrt_b - sqrt_a;
        if sqrt_a.is_zero() {
            return Err(SimError::InsufficientLiquidity);
        }
        if round_up {
            let inner =
                mul_div_rounding_up(numerator1, numerator2, sqrt_b).ok_or(SimError::Overflow)?;
            mul_div_rounding_up(inner, U256::from(1u64), sqrt_a).ok_or(SimError::Overflow)
        } else {
            let inner = mul_div(numerator1, numerator2, sqrt_b).ok_or(SimError::Overflow)?;
            Ok(inner / sqrt_a)
        }
    }

    /// SqrtPriceMath.getAmount1Delta (unsigned, with rounding flag).
    fn get_amount1_delta(
        mut sqrt_a: U256,
        mut sqrt_b: U256,
        liquidity: u128,
        round_up: bool,
    ) -> Result<U256, SimError> {
        if sqrt_a > sqrt_b {
            std::mem::swap(&mut sqrt_a, &mut sqrt_b);
        }
        let l = U256::from(liquidity);
        let diff = sqrt_b - sqrt_a;
        if round_up {
            mul_div_rounding_up(l, diff, q96()).ok_or(SimError::Overflow)
        } else {
            mul_div(l, diff, q96()).ok_or(SimError::Overflow)
        }
    }

    fn get_next_sqrt_price_from_amount0_rounding_up(
        sqrt_p: U256,
        liquidity: u128,
        amount: U256,
        add: bool,
    ) -> Result<U256, SimError> {
        if amount.is_zero() {
            return Ok(sqrt_p);
        }
        let numerator1 = U256::from(liquidity) << 96usize;
        if add {
            // product = amount * sqrt_p ; if no overflow use precise denom.
            // checked_add doubles as the solidity `denominator >= numerator1`
            // overflow guard that selects the fallback path.
            if let Some(product) = amount.checked_mul(sqrt_p) {
                if let Some(denominator) = numerator1.checked_add(product) {
                    return mul_div_rounding_up(numerator1, sqrt_p, denominator)
                        .ok_or(SimError::Overflow);
                }
            }
            // fallback: numerator1 / (numerator1/sqrt_p + amount)
            let denom = (numerator1 / sqrt_p)
                .checked_add(amount)
                .ok_or(SimError::Overflow)?;
            // divRoundingUp
            Ok(div_rounding_up(numerator1, denom))
        } else {
            // subtraction path (exact output) — not used for exact-input swaps
            let product = amount.checked_mul(sqrt_p).ok_or(SimError::Overflow)?;
            let denominator = numerator1
                .checked_sub(product)
                .ok_or(SimError::Overflow)?;
            mul_div_rounding_up(numerator1, sqrt_p, denominator).ok_or(SimError::Overflow)
        }
    }

    fn get_next_sqrt_price_from_amount1_rounding_down(
        sqrt_p: U256,
        liquidity: u128,
        amount: U256,
        add: bool,
    ) -> Result<U256, SimError> {
        let l = U256::from(liquidity);
        if add {
            let quotient = mul_div(amount, q96(), l).ok_or(SimError::Overflow)?;
            sqrt_p.checked_add(quotient).ok_or(SimError::Overflow)
        } else {
            let quotient =
                mul_div_rounding_up(amount, q96(), l).ok_or(SimError::Overflow)?;
            sqrt_p.checked_sub(quotient).ok_or(SimError::Overflow)
        }
    }

    fn get_next_sqrt_price_from_input(
        sqrt_p: U256,
        liquidity: u128,
        amount_in: U256,
        zero_for_one: bool,
    ) -> Result<U256, SimError> {
        if zero_for_one {
            Self::get_next_sqrt_price_from_amount0_rounding_up(sqrt_p, liquidity, amount_in, true)
        } else {
            Self::get_next_sqrt_price_from_amount1_rounding_down(sqrt_p, liquidity, amount_in, true)
        }
    }

    /// SwapMath.computeSwapStep for an exact-input step.
    /// Returns (sqrt_price_next, amount_in_used, amount_out, fee_amount).
    fn compute_swap_step(
        sqrt_current: U256,
        sqrt_target: U256,
        liquidity: u128,
        amount_remaining: U256,
        fee_pips: u32,
    ) -> Result<(U256, U256, U256, U256), SimError> {
        let zero_for_one = sqrt_current >= sqrt_target;
        let fee_pips_u = U256::from(fee_pips);
        let pips = U256::from(1_000_000u64);

        let amount_remaining_less_fee =
            mul_div(amount_remaining, pips - fee_pips_u, pips).ok_or(SimError::Overflow)?;

        let amount_in_to_target = if zero_for_one {
            Self::get_amount0_delta(sqrt_target, sqrt_current, liquidity, true)?
        } else {
            Self::get_amount1_delta(sqrt_current, sqrt_target, liquidity, true)?
        };

        let sqrt_next = if amount_remaining_less_fee >= amount_in_to_target {
            sqrt_target
        } else {
            Self::get_next_sqrt_price_from_input(
                sqrt_current,
                liquidity,
                amount_remaining_less_fee,
                zero_for_one,
            )?
        };

        let max = sqrt_target == sqrt_next;

        let (amount_in, amount_out) = if zero_for_one {
            let ain = if max {
                amount_in_to_target
            } else {
                Self::get_amount0_delta(sqrt_next, sqrt_current, liquidity, true)?
            };
            let aout = Self::get_amount1_delta(sqrt_next, sqrt_current, liquidity, false)?;
            (ain, aout)
        } else {
            let ain = if max {
                amount_in_to_target
            } else {
                Self::get_amount1_delta(sqrt_current, sqrt_next, liquidity, true)?
            };
            let aout = Self::get_amount0_delta(sqrt_current, sqrt_next, liquidity, false)?;
            (ain, aout)
        };

        let fee_amount = if !max {
            // used the entire remaining input; the rest is fee
            amount_remaining
                .checked_sub(amount_in)
                .ok_or(SimError::Overflow)?
        } else {
            mul_div_rounding_up(amount_in, fee_pips_u, pips - fee_pips_u)
                .ok_or(SimError::Overflow)?
        };

        Ok((sqrt_next, amount_in, amount_out, fee_amount))
    }

    /// v3-core `TickBitmap.nextInitializedTickWithinOneWord`: the next tick
    /// boundary within the CURRENT bitmap word (≤256 compressed ticks away) and
    /// whether it is initialized. Replicating the word-bounded step is required
    /// for wei-exactness: the on-chain swap rounds at each word boundary, so a
    /// sim that jumps directly between initialized ticks under-accumulates the
    /// per-step rounding and over-quotes.
    fn next_tick_within_word(&self, tick: i32, lte: bool) -> (i32, bool) {
        let spacing = self.tick_spacing;
        let mut compressed = tick / spacing;
        if tick % spacing != 0 && tick < 0 {
            compressed -= 1; // floor toward -inf, matching Solidity
        }
        // `ticks` is sorted ascending (see `liquidity_net_at`), so the
        // greatest/least initialized tick in `[lo, hi]` is found with a binary
        // search — O(log n) per step instead of an O(n) scan, which matters
        // because this runs once per tick-crossing iteration of the swap loop.
        if lte {
            let word_pos = compressed >> 8;
            let lo = (word_pos << 8) * spacing; // lowest tick in this word
            let hi = compressed * spacing; // current tick rounded down to spacing
            // greatest tick <= hi; initialized iff it is also >= lo.
            let idx = self.ticks.partition_point(|t| t.tick <= hi);
            match idx.checked_sub(1).map(|i| &self.ticks[i]) {
                Some(t) if t.tick >= lo => (t.tick, true),
                _ => (lo, false),
            }
        } else {
            let cp1 = compressed + 1;
            let word_pos = cp1 >> 8;
            let lo = cp1 * spacing;
            let hi = ((word_pos << 8) + 255) * spacing; // highest tick in word
            // least tick >= lo; initialized iff it is also <= hi.
            let idx = self.ticks.partition_point(|t| t.tick < lo);
            match self.ticks.get(idx) {
                Some(t) if t.tick <= hi => (t.tick, true),
                _ => (hi, false),
            }
        }
    }

    fn liquidity_net_at(&self, tick: i32) -> i128 {
        match self.ticks.binary_search_by_key(&tick, |t| t.tick) {
            Ok(i) => self.ticks[i].liquidity_net,
            Err(_) => 0,
        }
    }
}

fn div_rounding_up(a: U256, b: U256) -> U256 {
    let q = a / b;
    if (a % b).is_zero() {
        q
    } else {
        q + U256::from(1u64)
    }
}

impl Pool for UniV3Pool {
    fn address(&self) -> Address {
        self.address
    }

    fn protocol(&self) -> Protocol {
        Protocol::UniswapV3
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
        let zero_for_one = if token_in == self.token0 && token_out == self.token1 {
            true
        } else if token_in == self.token1 && token_out == self.token0 {
            false
        } else if token_in != self.token0 && token_in != self.token1 {
            return Err(SimError::UnknownToken(token_in));
        } else {
            return Err(SimError::UnknownToken(token_out));
        };

        // Price limit = chain bound, OR the window bound for windowed state so a
        // swap that would exit the known range stops at the edge (detected below)
        // instead of pretending the unfetched region is empty.
        let sqrt_price_limit = if zero_for_one {
            let chain = min_sqrt_ratio() + U256::from(1u64);
            match self.known_range {
                Some((lo, _)) if lo > MIN_TICK => Self::get_sqrt_ratio_at_tick(lo)?.max(chain),
                _ => chain,
            }
        } else {
            let chain = max_sqrt_ratio() - U256::from(1u64);
            match self.known_range {
                Some((_, hi)) if hi < MAX_TICK => Self::get_sqrt_ratio_at_tick(hi)?.min(chain),
                _ => chain,
            }
        };

        let mut amount_remaining = amount_in;
        let mut amount_calculated = U256::ZERO; // amount out accumulated
        let mut sqrt_price = self.sqrt_price_x96;
        let mut tick = self.tick;
        let mut liquidity = self.liquidity;

        let mut guard = 0u32;
        while !amount_remaining.is_zero() && sqrt_price != sqrt_price_limit {
            guard += 1;
            if guard > 10_000 {
                return Err(SimError::BadConfig("swap did not converge"));
            }

            let (raw_next, mut initialized) = self.next_tick_within_word(tick, zero_for_one);
            let tick_next = raw_next.clamp(MIN_TICK, MAX_TICK);
            if tick_next != raw_next {
                initialized = false; // clamped to a range bound, not a real tick
            }

            let sqrt_price_next_tick = Self::get_sqrt_ratio_at_tick(tick_next)?;
            // clamp target to the price limit
            let sqrt_target = if zero_for_one {
                if sqrt_price_next_tick < sqrt_price_limit {
                    sqrt_price_limit
                } else {
                    sqrt_price_next_tick
                }
            } else if sqrt_price_next_tick > sqrt_price_limit {
                sqrt_price_limit
            } else {
                sqrt_price_next_tick
            };

            if liquidity == 0 {
                // No active liquidity: jump straight to the boundary price.
                sqrt_price = sqrt_target;
            } else {
                let (sqrt_next, amount_in_step, amount_out_step, fee_amount) =
                    Self::compute_swap_step(
                        sqrt_price,
                        sqrt_target,
                        liquidity,
                        amount_remaining,
                        self.fee_pips,
                    )?;
                // For exact-input, consumed never exceeds amount_remaining, but
                // saturate defensively against off-by-one rounding.
                let consumed = amount_in_step
                    .checked_add(fee_amount)
                    .ok_or(SimError::Overflow)?;
                amount_remaining = amount_remaining.saturating_sub(consumed);
                amount_calculated += amount_out_step;
                sqrt_price = sqrt_next;
            }

            if sqrt_price == sqrt_price_next_tick {
                // Reached the word/tick boundary. Cross liquidity only if the
                // boundary tick is initialized; otherwise just step past it.
                if initialized {
                    let mut net = self.liquidity_net_at(tick_next);
                    if zero_for_one {
                        net = -net;
                    }
                    liquidity = apply_liquidity_net(liquidity, net)?;
                }
                tick = if zero_for_one { tick_next - 1 } else { tick_next };
            } else {
                // Input exhausted before the boundary — done.
                break;
            }
        }

        // Windowed state: if we stopped at the window edge with input left, the
        // trade exceeds our fetched ticks — signal rather than return a wrong value.
        if self.known_range.is_some()
            && !amount_remaining.is_zero()
            && sqrt_price == sqrt_price_limit
        {
            return Err(SimError::IncompleteState);
        }

        Ok(amount_calculated)
    }

    fn gas_estimate(&self) -> u64 {
        // V3 swaps vary with ticks crossed; ~120k base + crossings.
        let crossings = 2u64;
        120_000 + crossings * 20_000
    }
}

fn apply_liquidity_net(liquidity: u128, net: i128) -> Result<u128, SimError> {
    if net >= 0 {
        liquidity
            .checked_add(net as u128)
            .ok_or(SimError::Overflow)
    } else {
        liquidity
            .checked_sub(net.unsigned_abs())
            .ok_or(SimError::InsufficientLiquidity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(s: &str) -> U256 {
        U256::from_str_radix(s, 10).unwrap()
    }

    // encodePriceSqrt(1, 1) == 2^96
    fn q96_val() -> U256 {
        dec("79228162514264337593543950336")
    }
    // encodePriceSqrt(121, 100) == floor(1.1 * 2^96), from v3-core tests.
    fn sqrt_121_100() -> U256 {
        dec("87150978765690771352898345369")
    }
    fn e18() -> u128 {
        1_000_000_000_000_000_000u128
    }

    #[test]
    fn sqrt_ratio_at_tick_zero() {
        // tick 0 => price 1 => sqrtPriceX96 == 2^96
        assert_eq!(UniV3Pool::get_sqrt_ratio_at_tick(0).unwrap(), q96());
    }

    // ---- TickMath.getSqrtRatioAtTick: exact vectors from Uniswap v3-core ----
    // github.com/Uniswap/v3-core test/TickMath.spec.ts
    #[test]
    fn tick_math_official_vectors() {
        assert_eq!(
            UniV3Pool::get_sqrt_ratio_at_tick(MIN_TICK).unwrap(),
            dec("4295128739")
        );
        assert_eq!(
            UniV3Pool::get_sqrt_ratio_at_tick(MIN_TICK + 1).unwrap(),
            dec("4295343490")
        );
        assert_eq!(
            UniV3Pool::get_sqrt_ratio_at_tick(MAX_TICK - 1).unwrap(),
            dec("1461373636630004318706518188784493106690254656249")
        );
        assert_eq!(
            UniV3Pool::get_sqrt_ratio_at_tick(MAX_TICK).unwrap(),
            dec("1461446703485210103287273052203988822378723970342")
        );
    }

    // ---- SqrtPriceMath.getAmount0Delta / getAmount1Delta: v3-core vectors ----
    // github.com/Uniswap/v3-core test/SqrtPriceMath.spec.ts
    #[test]
    fn amount_deltas_official_vectors() {
        // getAmount0Delta(1, 1.21, 1e18)
        assert_eq!(
            UniV3Pool::get_amount0_delta(q96_val(), sqrt_121_100(), e18(), true).unwrap(),
            dec("90909090909090910")
        );
        assert_eq!(
            UniV3Pool::get_amount0_delta(q96_val(), sqrt_121_100(), e18(), false).unwrap(),
            dec("90909090909090909")
        );
        // getAmount1Delta(1, 1.21, 1e18)
        assert_eq!(
            UniV3Pool::get_amount1_delta(q96_val(), sqrt_121_100(), e18(), true).unwrap(),
            dec("100000000000000000")
        );
        assert_eq!(
            UniV3Pool::get_amount1_delta(q96_val(), sqrt_121_100(), e18(), false).unwrap(),
            dec("99999999999999999")
        );
    }

    // ---- SqrtPriceMath.getNextSqrtPriceFromInput: v3-core vectors ----
    #[test]
    fn next_sqrt_price_from_input_official_vectors() {
        // (price 1, L=1e18, amountIn=1e17, oneForZero) => add to amount1
        assert_eq!(
            UniV3Pool::get_next_sqrt_price_from_input(
                q96_val(),
                e18(),
                dec("100000000000000000"),
                false,
            )
            .unwrap(),
            dec("87150978765690771352898345369")
        );
        // (price 1, L=1e18, amountIn=1e17, zeroForOne)
        assert_eq!(
            UniV3Pool::get_next_sqrt_price_from_input(
                q96_val(),
                e18(),
                dec("100000000000000000"),
                true,
            )
            .unwrap(),
            dec("72025602285694852357767227579")
        );
        // (price 1, L=1e19, amountIn=2^100, zeroForOne)
        assert_eq!(
            UniV3Pool::get_next_sqrt_price_from_input(
                q96_val(),
                10_000_000_000_000_000_000u128,
                dec("1267650600228229401496703205376"), // 2^100
                true,
            )
            .unwrap(),
            dec("624999999995069620")
        );
        // Overflow-path sanity: tiny liquidity, huge input rounds price to 1.
        assert_eq!(
            UniV3Pool::get_next_sqrt_price_from_input(
                U256::from(1u64),
                1u128,
                U256::MAX / U256::from(2u64),
                true,
            )
            .unwrap(),
            U256::from(1u64)
        );
    }

    // The "swap computation" vector from v3-core SqrtPriceMath.spec.ts.
    #[test]
    fn next_sqrt_price_swap_computation_vector() {
        let sqrt_p = dec("1025574284609383690408304870162715216695788925244");
        let liquidity = 50_015_962_439_936_049_619_261_659_728_067_971_248u128;
        let got =
            UniV3Pool::get_next_sqrt_price_from_input(sqrt_p, liquidity, U256::from(406u64), true)
                .unwrap();
        assert_eq!(
            got,
            dec("1025574284609383582644711336373707553698163132913")
        );
    }

    // ---- End-to-end swap: single active range, no tick crossing ----
    #[test]
    fn windowed_state_matches_within_window_and_flags_beyond() {
        // Full pool: liquidity across [-60000, 60000].
        let t0 = Address::repeat_byte(1);
        let t1 = Address::repeat_byte(2);
        let ticks = vec![
            TickData { tick: -60_000, liquidity_net: 5_000_000_000_000_000_000i128 },
            TickData { tick: 60_000, liquidity_net: -5_000_000_000_000_000_000i128 },
        ];
        let full = UniV3Pool::new(
            Address::repeat_byte(9), t0, t1, 500, q96_val(),
            5_000_000_000_000_000_000u128, 0, 10, ticks.clone(),
        );
        // Windowed copy that only "knows" ticks within [-600, 600].
        let windowed = UniV3Pool::new(
            Address::repeat_byte(9), t0, t1, 500, q96_val(),
            5_000_000_000_000_000_000u128, 0, 10, ticks,
        )
        .with_known_range(-600, 600);

        // A small trade stays well within the window -> identical to full data.
        let small = dec("1000000000000000"); // 1e15
        assert_eq!(
            windowed.quote(t0, t1, small).unwrap(),
            full.quote(t0, t1, small).unwrap()
        );

        // A trade large enough to push price past the window -> flagged, not wrong.
        let big = dec("100000000000000000000"); // 100e18
        assert_eq!(windowed.quote(t0, t1, big), Err(SimError::IncompleteState));
        // ...while the full-data pool still quotes it fine.
        assert!(full.quote(t0, t1, big).is_ok());
    }

    #[test]
    fn single_range_swap_consumes_input() {
        // A pool at price 1 with a single wide liquidity range. Swapping a small
        // amount in should return a positive, slightly-less-than-input amount.
        let t0 = Address::repeat_byte(1);
        let t1 = Address::repeat_byte(2);
        let ticks = vec![
            TickData { tick: -60_000, liquidity_net: 1_000_000_000_000_000_000i128 },
            TickData { tick: 60_000, liquidity_net: -1_000_000_000_000_000_000i128 },
        ];
        let pool = UniV3Pool::new(
            Address::repeat_byte(9),
            t0,
            t1,
            500, // 0.05%
            q96_val(),
            1_000_000_000_000_000_000u128,
            0,
            10, // tick_spacing
            ticks,
        );
        let amount_in = dec("1000000000000000"); // 1e15
        let out = pool.quote(t0, t1, amount_in).unwrap();
        assert!(out > U256::ZERO && out < amount_in, "out={out}");
    }
}
