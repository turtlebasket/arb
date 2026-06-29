//! The [`Pool`] abstraction: anything that can quote a swap.
//!
//! Every concrete AMM (Uniswap V2/V3, Solidly, Curve, Balancer) implements
//! [`Pool`]. The arb engine only ever sees this trait, so adding a new protocol
//! never touches the routing/scanning layer.

use crate::types::{Address, U256};

/// Which AMM family a pool belongs to. Used for gas estimation, debugging, and
/// grouping. Concrete forks (Pancake, SunSwap, Aerodrome, ...) map onto one of
/// these math families.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Protocol {
    /// Constant-product `x*y=k` (Uniswap V2, Pancake V2, SunSwap, Solidly-volatile).
    UniswapV2,
    /// Concentrated liquidity (Uniswap V3, Pancake V3, Aerodrome Slipstream).
    UniswapV3,
    /// Solidly stable curve `x^3*y + x*y^3 = k` (Aerodrome/Velodrome stable).
    SolidlyStable,
    /// Curve StableSwap invariant.
    CurveStable,
    /// Balancer weighted pool.
    BalancerWeighted,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SimError {
    #[error("token {0} is not part of this pool")]
    UnknownToken(Address),
    #[error("token_in and token_out are the same")]
    SameToken,
    #[error("amount in is zero")]
    ZeroAmount,
    #[error("insufficient liquidity for the requested swap")]
    InsufficientLiquidity,
    #[error("arithmetic overflow in pool math")]
    Overflow,
    #[error("pool misconfigured: {0}")]
    BadConfig(&'static str),
    /// The cached tick window does not cover this swap (V3 windowed state): the
    /// trade would move price beyond the fetched range. Widen the window and
    /// refetch. Never returned with a (silently wrong) value.
    #[error("incomplete state: swap exceeds the cached tick window")]
    IncompleteState,
}

/// A simulatable liquidity pool.
///
/// Quotes are pure functions of the pool's cached state — no I/O. State is
/// expected to be refreshed externally (from chain reads) before simulation.
pub trait Pool: std::fmt::Debug {
    /// On-chain address (pool/pair contract). Identity for dedup & routing.
    fn address(&self) -> Address;

    /// The AMM family.
    fn protocol(&self) -> Protocol;

    /// Tokens tradeable in this pool.
    fn tokens(&self) -> &[Address];

    /// Simulate swapping exactly `amount_in` of `token_in` for `token_out`,
    /// returning the output amount **net of the pool's swap fee**.
    ///
    /// This is the exact-output-given-input quote and must match on-chain
    /// behavior so that downstream profit calculations are trustworthy.
    fn quote(
        &self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Result<U256, SimError>;

    /// Rough gas cost (in gas units) of executing one swap through this pool.
    /// The scanner multiplies this by the live gas price to convert a gross
    /// edge into a net, gas-adjusted edge. These are order-of-magnitude
    /// estimates per AMM family, refined later against real execution traces.
    fn gas_estimate(&self) -> u64;

    /// Convenience: does this pool trade `token`?
    fn has_token(&self, token: Address) -> bool {
        self.tokens().contains(&token)
    }
}
