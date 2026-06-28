//! Chain event/state model shared by the streaming engine and its sources.
//!
//! Events carry **absolute** post-event pool state where the protocol allows it
//! (UniV2/Solidly `Sync`, UniV3 `Swap` price/liquidity scalars), so applying an
//! event is idempotent and last-write-wins within a block. Relative-only
//! mutations (UniV3 `Mint`/`Burn`, Curve/Balancer deltas) must be applied in
//! strict `(block, log_index)` order and none may be skipped — which is why the
//! engine guarantees gapless, ordered delivery.

use crate::types::{Address, B256, U256};

/// Absolute cached state of a pool (extend per AMM family).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PoolState {
    UniV2 { reserve0: U256, reserve1: U256 },
    UniV3 { sqrt_price_x96: U256, liquidity: u128, tick: i32 },
}

/// A decoded, state-changing pool event in global stream order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolEvent {
    pub pool: Address,
    pub block: u64,
    pub log_index: u64,
    pub kind: EventKind,
}

impl PoolEvent {
    /// Total order key used for dedup and ordering.
    pub fn key(&self) -> (u64, u64) {
        (self.block, self.log_index)
    }

    /// Apply this event to a cached state, producing the new state.
    /// Absolute events (the only kinds modeled here) overwrite.
    pub fn apply(&self, _prev: &PoolState) -> PoolState {
        match &self.kind {
            EventKind::SyncV2 { reserve0, reserve1 } => PoolState::UniV2 {
                reserve0: *reserve0,
                reserve1: *reserve1,
            },
            EventKind::SwapV3 {
                sqrt_price_x96,
                liquidity,
                tick,
            } => PoolState::UniV3 {
                sqrt_price_x96: *sqrt_price_x96,
                liquidity: *liquidity,
                tick: *tick,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventKind {
    /// UniV2/Solidly `Sync(uint112 reserve0, uint112 reserve1)` — absolute.
    SyncV2 { reserve0: U256, reserve1: U256 },
    /// UniV3 `Swap(...)` price/liquidity/tick scalars — absolute for these fields.
    SwapV3 {
        sqrt_price_x96: U256,
        liquidity: u128,
        tick: i32,
    },
}

/// An item delivered by a [`crate::live::source::ChainSource`] subscription
/// (chain-agnostic: sealed blocks + their pool events).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainItem {
    NewHead {
        number: u64,
        hash: B256,
        parent_hash: B256,
    },
    Event(PoolEvent),
}
