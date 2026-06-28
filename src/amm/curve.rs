//! Curve StableSwap plain pools.
//!
//! Port of the canonical `get_dy` path: compute the invariant `D` for the
//! current balances, then solve for the new balance `y` of the output coin,
//! both via Newton's method. Balances are held in a common 1e18 precision via
//! per-coin rate multipliers, exactly like the Vyper reference.

use crate::types::{Address, U256};

use crate::math::mul_div;
use crate::pool::{Pool, Protocol, SimError};

#[derive(Debug, Clone)]
pub struct CurveStablePool {
    pub address: Address,
    /// Coin addresses, in pool index order.
    pub coins: Vec<Address>,
    /// Balances in native units, in coin order.
    pub balances: Vec<U256>,
    /// Rate multipliers that bring each coin to 1e18 precision:
    /// `rate[i] = 1e18 * 10**(18 - decimals[i])` for a plain pool.
    pub rates: Vec<U256>,
    /// Amplification coefficient `A` (already de-scaled, i.e. the human `A`).
    pub a: U256,
    /// Fee in basis points applied to the output (Curve fee is in 1e10 units
    /// on-chain; we accept bps here for consistency with other pools).
    pub fee_bps: u32,
}

impl CurveStablePool {
    pub fn new(
        address: Address,
        coins: Vec<Address>,
        balances: Vec<U256>,
        rates: Vec<U256>,
        a: U256,
        fee_bps: u32,
    ) -> Self {
        Self {
            address,
            coins,
            balances,
            rates,
            a,
            fee_bps,
        }
    }

    /// Convert raw balances to the common 1e18 precision (`xp` in the contract).
    fn xp(&self) -> Option<Vec<U256>> {
        self.balances
            .iter()
            .zip(self.rates.iter())
            .map(|(b, r)| mul_div(*r, *b, U256::from(1_000_000_000_000_000_000u64)))
            .collect()
    }

    /// Invariant D via Newton's method (matches StableSwap `get_D`).
    fn get_d(xp: &[U256], a: U256) -> Option<U256> {
        let n = U256::from(xp.len() as u64);
        let mut s = U256::ZERO;
        for x in xp {
            s = s.checked_add(*x)?;
        }
        if s.is_zero() {
            return Some(U256::ZERO);
        }
        let ann = a.checked_mul(n)?; // Ann = A * n  (reference uses A*n**n via A_PRECISION; see note)
        let mut d = s;
        for _ in 0..255 {
            // D_P = D^(n+1) / (n^n * prod(xp))
            let mut d_p = d;
            for x in xp {
                // d_p = d_p * D / (x * n)
                let denom = x.checked_mul(n)?;
                if denom.is_zero() {
                    return None;
                }
                d_p = mul_div(d_p, d, denom)?;
            }
            let d_prev = d;
            // D = (Ann*S + D_P*n) * D / ((Ann-1)*D + (n+1)*D_P)
            let num = ann
                .checked_mul(s)?
                .checked_add(d_p.checked_mul(n)?)?;
            let denom = ann
                .checked_sub(U256::from(1u64))?
                .checked_mul(d)?
                .checked_add(d_p.checked_mul(n + U256::from(1u64))?)?;
            d = mul_div(num, d, denom)?;
            if d > d_prev {
                if d - d_prev <= U256::from(1u64) {
                    return Some(d);
                }
            } else if d_prev - d <= U256::from(1u64) {
                return Some(d);
            }
        }
        Some(d)
    }

    /// Solve for new balance `y` of coin `j` given coin `i` set to `x` (`get_y`).
    fn get_y(
        i: usize,
        j: usize,
        x: U256,
        xp: &[U256],
        a: U256,
    ) -> Option<U256> {
        let n = U256::from(xp.len() as u64);
        let d = Self::get_d(xp, a)?;
        let ann = a.checked_mul(n)?;
        let mut c = d;
        let mut s = U256::ZERO;
        for (idx, xp_k) in xp.iter().enumerate() {
            let x_k = if idx == i {
                x
            } else if idx == j {
                continue;
            } else {
                *xp_k
            };
            s = s.checked_add(x_k)?;
            // c = c * D / (x_k * n)
            c = mul_div(c, d, x_k.checked_mul(n)?)?;
        }
        // c = c * D / (Ann * n)
        c = mul_div(c, d, ann.checked_mul(n)?)?;
        // b = S + D/Ann
        let b = s.checked_add(d.checked_div(ann)?)?;
        let mut y = d;
        for _ in 0..255 {
            let y_prev = y;
            // y = (y^2 + c) / (2y + b - D)
            let num = mul_div(y, y, U256::from(1u64))?.checked_add(c)?;
            let denom = y
                .checked_mul(U256::from(2u64))?
                .checked_add(b)?
                .checked_sub(d)?;
            y = num.checked_div(denom)?;
            if y > y_prev {
                if y - y_prev <= U256::from(1u64) {
                    return Some(y);
                }
            } else if y_prev - y <= U256::from(1u64) {
                return Some(y);
            }
        }
        Some(y)
    }

    fn index_of(&self, token: Address) -> Option<usize> {
        self.coins.iter().position(|c| *c == token)
    }
}

impl Pool for CurveStablePool {
    fn address(&self) -> Address {
        self.address
    }

    fn protocol(&self) -> Protocol {
        Protocol::CurveStable
    }

    fn tokens(&self) -> &[Address] {
        &self.coins
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
        if self.coins.len() != self.balances.len() || self.coins.len() != self.rates.len() {
            return Err(SimError::BadConfig("coins/balances/rates length mismatch"));
        }
        let i = self
            .index_of(token_in)
            .ok_or(SimError::UnknownToken(token_in))?;
        let j = self
            .index_of(token_out)
            .ok_or(SimError::UnknownToken(token_out))?;

        let xp = self.xp().ok_or(SimError::Overflow)?;
        let one = U256::from(1_000_000_000_000_000_000u64);
        // x = xp[i] + dx * rate_i / 1e18
        let dx_xp = mul_div(amount_in, self.rates[i], one).ok_or(SimError::Overflow)?;
        let x = xp[i].checked_add(dx_xp).ok_or(SimError::Overflow)?;

        let y = Self::get_y(i, j, x, &xp, self.a).ok_or(SimError::Overflow)?;
        // dy = (xp[j] - y - 1) , minus fee, then back to native units
        let dy_xp = xp[j]
            .checked_sub(y)
            .ok_or(SimError::InsufficientLiquidity)?
            .checked_sub(U256::from(1u64))
            .ok_or(SimError::InsufficientLiquidity)?;
        let fee = mul_div(dy_xp, U256::from(self.fee_bps), U256::from(10_000u64))
            .ok_or(SimError::Overflow)?;
        let dy_after_fee = dy_xp - fee;
        // back to native units of coin j
        mul_div(dy_after_fee, one, self.rates[j]).ok_or(SimError::Overflow)
    }

    fn gas_estimate(&self) -> u64 {
        150_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(n: u8) -> Address {
        Address::repeat_byte(n)
    }

    fn e18() -> U256 {
        U256::from(1_000_000_000_000_000_000u64)
    }

    #[test]
    fn three_pool_stable_quote() {
        // 3 coins, all 18 decimals, balanced 1M each, A=100, 4bps fee.
        let one_m = U256::from(1_000_000u64) * e18();
        let pool = CurveStablePool::new(
            addr(9),
            vec![addr(1), addr(2), addr(3)],
            vec![one_m, one_m, one_m],
            vec![e18(), e18(), e18()],
            U256::from(100u64),
            4,
        );
        let amount_in = U256::from(10_000u64) * e18();
        let out = pool.quote(addr(1), addr(2), amount_in).unwrap();
        // Near 1:1 with tiny slippage+fee.
        let lo = mul_div(amount_in, U256::from(995u64), U256::from(1000u64)).unwrap();
        assert!(out < amount_in && out > lo, "out={out} in={amount_in}");
    }

    // The defining correctness property of StableSwap: a swap must conserve the
    // invariant D. We compute D, perform the get_y step the quote uses, then
    // recompute D on the post-swap balances and require it to match (the Newton
    // solvers leave at most a couple of wei of rounding error). This validates
    // get_d/get_y the same way Curve's own test suite asserts on D.
    #[test]
    fn swap_conserves_invariant_d() {
        let one_m = U256::from(1_000_000u64) * e18();
        let xp = vec![one_m, one_m, one_m];
        let a = U256::from(2000u64);
        let d0 = CurveStablePool::get_d(&xp, a).unwrap();

        // Push 50k of coin 0 in, solve for coin 1's new balance.
        let dx = U256::from(50_000u64) * e18();
        let x = xp[0] + dx;
        let y = CurveStablePool::get_y(0, 1, x, &xp, a).unwrap();

        let xp_after = vec![x, y, xp[2]];
        let d1 = CurveStablePool::get_d(&xp_after, a).unwrap();

        let diff = if d1 > d0 { d1 - d0 } else { d0 - d1 };
        assert!(diff <= U256::from(2u64), "D drifted by {diff} (d0={d0}, d1={d1})");
    }
}
